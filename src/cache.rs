//! Remembered destination listings.
//!
//! A cache generation is trusted only while the marker in the destination root
//! holds the same run id. A run overwrites the marker when it starts and writes
//! the new id only after a clean finish, so an interrupted run, another machine
//! or a replaced disk all lead to a full scan.
//!
//! Each saved file is a log, to spare the disk it lives on: the first record
//! holds everything, and later runs append only what changed. Rewriting the file
//! every run would cost about 90 MB per million files.
//!
//! Every record is complete and checksummed, and states what it is valid for: a
//! run id or a journal position. Losing the end of a file therefore leaves an
//! older state that is still true for what it says: the marker no longer matches
//! and the destination is listed, or the journal is read again from the older
//! position. That is why appending needs no flush.

use crate::journal::{Cursor, SourceIndex};
use crate::model::{Entry, Kind, MARKER_NAME};
use crate::platform;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"HKGMCCH3";
const SOURCE_MAGIC: &[u8; 8] = b"HKGMSRC3";

/// Appended bytes allowed before a file is rewritten, however small it is.
const MIN_LOG_BYTES: u64 = 256 * 1024;

pub type DirMap = HashMap<PathBuf, Vec<Entry>>;

/// Where a saved file's readable records end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogPosition {
    /// End of the last good record. Anything after it was torn off by a crash.
    valid_len: u64,
    /// End of the first record, which holds everything.
    base_len: u64,
}

pub struct CacheFile {
    pub run_id: String,
    pub volume_id: String,
    /// Unix seconds of the last run that listed the whole destination.
    pub last_full: u64,
    pub dirs: DirMap,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", std::process::id())
}

pub fn marker_path(dest: &Path) -> PathBuf {
    dest.join(MARKER_NAME)
}

pub fn read_marker(dest: &Path) -> Option<String> {
    fs::read_to_string(marker_path(dest))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn write_marker(dest: &Path, value: &str) -> io::Result<()> {
    fs::write(marker_path(dest), value)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Cache file for a source and destination pair.
pub fn cache_path(cache_dir: &Path, source: &Path, dest: &Path) -> PathBuf {
    let mut id = platform::os_to_bytes(source.as_os_str());
    id.push(0);
    id.extend(platform::os_to_bytes(dest.as_os_str()));
    cache_dir.join(format!("{:016x}.bin", fnv1a(&id)))
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u64).to_le_bytes());
    out.extend(bytes);
}

fn put_listing(out: &mut Vec<u8>, rel: &Path, entries: &[Entry]) {
    put_bytes(out, &platform::os_to_bytes(rel.as_os_str()));
    out.extend((entries.len() as u64).to_le_bytes());
    for e in entries {
        put_bytes(out, &platform::os_to_bytes(&e.name));
        out.push(match e.kind {
            Kind::File => 0,
            Kind::Dir => 1,
            Kind::Link => 2,
            Kind::DirLink => 3,
        });
        out.extend(e.size.to_le_bytes());
        out.extend(e.mtime.to_le_bytes());
        out.extend(e.created.to_le_bytes());
        out.extend(u64::from(e.attrs).to_le_bytes());
    }
}

/// A framed record: its length, the head, the listings to set, the folders to
/// forget, and a checksum over all of those.
fn record<'a>(
    head: &[u8],
    listings: impl ExactSizeIterator<Item = (&'a Path, &'a [Entry])>,
    removed: &[&Path],
) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    out.extend(head);
    out.extend((listings.len() as u64).to_le_bytes());
    for (rel, entries) in listings {
        put_listing(&mut out, rel, entries);
    }
    out.extend((removed.len() as u64).to_le_bytes());
    for rel in removed {
        put_bytes(&mut out, &platform::os_to_bytes(rel.as_os_str()));
    }
    let payload = (out.len() - 8) as u64;
    out[..8].copy_from_slice(&payload.to_le_bytes());
    let hash = fnv1a(&out);
    out.extend(hash.to_le_bytes());
    out
}

fn snapshot(magic: &[u8; 8], head: &[u8], dirs: &DirMap) -> Vec<u8> {
    let listings = dirs.iter().map(|(rel, e)| (rel.as_path(), e.as_slice()));
    let body = record(head, listings, &[]);
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend(magic);
    out.extend(body);
    out
}

fn cache_head(cache: &CacheFile) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, cache.run_id.as_bytes());
    put_bytes(&mut out, cache.volume_id.as_bytes());
    out.extend(cache.last_full.to_le_bytes());
    out
}

fn source_head(index: &SourceIndex) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(index.cursor.journal_id.to_le_bytes());
    out.extend(index.cursor.next_usn.to_le_bytes());
    out.extend(index.root.to_le_bytes());
    put_bytes(&mut out, index.volume_id.as_bytes());
    out
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }

    fn text(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
}

struct Replayed<H> {
    head: H,
    dirs: DirMap,
    position: LogPosition,
}

/// Applies the file's records in order, up to the first one that is incomplete
/// or damaged. A file without a good first record gives nothing.
fn replay<H>(
    data: &[u8],
    magic: &[u8; 8],
    read_head: fn(&mut Reader) -> Option<H>,
) -> Option<Replayed<H>> {
    if data.get(..8)? != magic {
        return None;
    }
    let mut pos = 8usize;
    let mut state: Option<Replayed<H>> = None;
    while let Some((payload, end)) = next_record(data, pos) {
        let mut r = Reader {
            data: payload,
            pos: 0,
        };
        // Parsed in full before any of it is applied, so that a record counts
        // entirely or not at all.
        let Some(head) = read_head(&mut r) else { break };
        let Some(listings) = read_dirs(&mut r) else {
            break;
        };
        let Some(removed) = read_paths(&mut r) else {
            break;
        };
        if r.pos != payload.len() {
            break;
        }
        let end_u64 = end as u64;
        match &mut state {
            None => {
                state = Some(Replayed {
                    head,
                    dirs: listings,
                    position: LogPosition {
                        valid_len: end_u64,
                        base_len: end_u64,
                    },
                });
            }
            Some(state) => {
                state.head = head;
                for rel in removed {
                    state.dirs.remove(&rel);
                }
                state.dirs.extend(listings);
                state.position.valid_len = end_u64;
            }
        }
        pos = end;
    }
    state
}

/// The payload of the record at `pos` and where the record ends, if it is whole.
fn next_record(data: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let len_end = pos.checked_add(8)?;
    let len = u64::from_le_bytes(data.get(pos..len_end)?.try_into().ok()?);
    let payload_end = len_end.checked_add(usize::try_from(len).ok()?)?;
    let end = payload_end.checked_add(8)?;
    let stored = u64::from_le_bytes(data.get(payload_end..end)?.try_into().ok()?);
    if fnv1a(&data[pos..payload_end]) != stored {
        return None;
    }
    Some((&data[len_end..payload_end], end))
}

fn read_cache_head(r: &mut Reader) -> Option<(String, String, u64)> {
    Some((r.text()?, r.text()?, r.u64()?))
}

fn read_source_head(r: &mut Reader) -> Option<(Cursor, u64, String)> {
    let journal_id = r.u64()?;
    let next_usn = r.u64()? as i64;
    let root = r.u64()?;
    let volume_id = r.text()?;
    Some((
        Cursor {
            journal_id,
            next_usn,
        },
        root,
        volume_id,
    ))
}

fn decode(data: &[u8]) -> Option<CacheFile> {
    let state = replay(data, MAGIC, read_cache_head)?;
    let (run_id, volume_id, last_full) = state.head;
    Some(CacheFile {
        run_id,
        volume_id,
        last_full,
        dirs: state.dirs,
    })
}

fn read_paths(r: &mut Reader) -> Option<Vec<PathBuf>> {
    let count = r.u64()?;
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(PathBuf::from(platform::os_from_bytes(r.bytes()?)?));
    }
    Some(out)
}

fn read_dirs(r: &mut Reader) -> Option<DirMap> {
    let dir_count = r.u64()?;
    let mut dirs = HashMap::new();
    for _ in 0..dir_count {
        let rel = PathBuf::from(platform::os_from_bytes(r.bytes()?)?);
        let count = r.u64()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let name = platform::os_from_bytes(r.bytes()?)?;
            let kind = match r.take(1)?[0] {
                0 => Kind::File,
                1 => Kind::Dir,
                2 => Kind::Link,
                3 => Kind::DirLink,
                _ => return None,
            };
            let size = r.u64()?;
            let mtime = r.u64()? as i64;
            let created = r.u64()? as i64;
            let attrs = r.u64()? as u32;
            entries.push(Entry {
                name,
                kind,
                size,
                mtime,
                created,
                attrs,
            });
        }
        dirs.insert(rel, entries);
    }
    Some(dirs)
}

fn decode_source_index(data: &[u8]) -> Option<SourceIndex> {
    let state = replay(data, SOURCE_MAGIC, read_source_head)?;
    let (cursor, root, volume_id) = state.head;
    Some(SourceIndex {
        cursor,
        root,
        volume_id,
        dirs: state.dirs,
        unchanged_since: Some(state.position),
    })
}

pub fn load_source_index(path: &Path) -> Option<SourceIndex> {
    decode_source_index(&fs::read(path).ok()?)
}

/// Saves the source listings. When the journal named nothing since the file was
/// read, only the new position is added, without reading the file again.
pub fn save_source_index(path: &Path, index: &SourceIndex) -> io::Result<()> {
    let head = source_head(index);
    if let Some(position) = index.unchanged_since {
        let added = record(&head, std::iter::empty(), &[]);
        return add_record(path, SOURCE_MAGIC, position, &added, &head, &index.dirs);
    }
    save_log(path, SOURCE_MAGIC, read_source_head, &head, &index.dirs)
}

pub fn load(path: &Path) -> Option<CacheFile> {
    decode(&fs::read(path).ok()?)
}

pub fn save(path: &Path, cache: &CacheFile) -> io::Result<()> {
    save_log(
        path,
        MAGIC,
        read_cache_head,
        &cache_head(cache),
        &cache.dirs,
    )
}

/// Adds the difference from what the file holds, or starts the file afresh.
fn save_log<H>(
    path: &Path,
    magic: &[u8; 8],
    read_head: fn(&mut Reader) -> Option<H>,
    head: &[u8],
    dirs: &DirMap,
) -> io::Result<()> {
    let prior = fs::read(path)
        .ok()
        .and_then(|data| replay(&data, magic, read_head));
    let Some(prior) = prior else {
        return write_atomically(path, &snapshot(magic, head, dirs));
    };
    let changed = dirs.iter().filter_map(|(rel, entries)| {
        let same = prior
            .dirs
            .get(rel)
            .is_some_and(|old| same_listing(old, entries));
        (!same).then_some((rel.as_path(), entries.as_slice()))
    });
    let changed: Vec<_> = changed.collect();
    let removed: Vec<&Path> = prior
        .dirs
        .keys()
        .filter(|rel| !dirs.contains_key(*rel))
        .map(PathBuf::as_path)
        .collect();
    let added = record(head, changed.into_iter(), &removed);
    add_record(path, magic, prior.position, &added, head, dirs)
}

/// Whether two listings hold the same entries, in any order.
fn same_listing<'a>(a: &'a [Entry], b: &'a [Entry]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a == b {
        return true;
    }
    let sorted = |list: &'a [Entry]| {
        let mut refs: Vec<&Entry> = list.iter().collect();
        refs.sort_by(|x, y| x.name.cmp(&y.name));
        refs
    };
    sorted(a) == sorted(b)
}

/// Appends a record, unless the appended part has grown enough to be worth
/// folding into one. Allowing half the first record keeps the total written to
/// at most about three times what really changed, and reading quick.
fn add_record(
    path: &Path,
    magic: &[u8; 8],
    position: LogPosition,
    added: &[u8],
    head: &[u8],
    dirs: &DirMap,
) -> io::Result<()> {
    let appended = position.valid_len - position.base_len + added.len() as u64;
    let allowed = (position.base_len / 2).max(MIN_LOG_BYTES);
    if appended <= allowed && append_record(path, position.valid_len, added).is_ok() {
        return Ok(());
    }
    write_atomically(path, &snapshot(magic, head, dirs))
}

fn append_record(path: &Path, valid_len: u64, added: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    if file.metadata()?.len() < valid_len {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    // A torn tail must go, or everything after it could never be read back.
    file.set_len(valid_len)?;
    file.seek(SeekFrom::Start(valid_len))?;
    file.write_all(added)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Removes a cache that can no longer be trusted.
pub fn discard(path: &Path) {
    // A missing file is the wanted result, so the error carries no information.
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CacheFile {
        let mut dirs = DirMap::new();
        dirs.insert(
            PathBuf::from("a"),
            vec![Entry {
                name: "日本語.txt".into(),
                kind: Kind::File,
                size: 12,
                mtime: -5,
                created: 3,
                attrs: 1,
            }],
        );
        CacheFile {
            run_id: "r".into(),
            volume_id: "v".into(),
            last_full: 9,
            dirs,
        }
    }

    fn entry(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            kind: Kind::File,
            size,
            mtime: 1,
            created: 2,
            attrs: 0,
        }
    }

    /// A destination record with `folders` folders of 20 entries each.
    fn large(folders: usize) -> CacheFile {
        let mut dirs = DirMap::new();
        for f in 0..folders {
            let entries = (0..20).map(|i| entry(&format!("file{i}"), 1)).collect();
            dirs.insert(PathBuf::from(format!("folder{f}")), entries);
        }
        CacheFile {
            run_id: "first".into(),
            volume_id: "v".into(),
            last_full: 1,
            dirs,
        }
    }

    fn file_len(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    fn encoded(cache: &CacheFile) -> Vec<u8> {
        snapshot(MAGIC, &cache_head(cache), &cache.dirs)
    }

    #[test]
    fn round_trip() {
        let back = decode(&encoded(&sample())).unwrap();
        assert_eq!(back.run_id, "r");
        assert_eq!(back.last_full, 9);
        let e = &back.dirs[&PathBuf::from("a")][0];
        assert_eq!(e.name, "日本語.txt");
        assert_eq!(e.mtime, -5);
        assert_eq!(e.created, 3);
        assert_eq!(e.attrs, 1);
        assert_eq!(e.kind, Kind::File);
    }

    #[test]
    fn source_index_round_trip() {
        let index = SourceIndex {
            cursor: Cursor {
                journal_id: 7,
                next_usn: 1234,
            },
            root: 99,
            volume_id: "vol".into(),
            dirs: sample().dirs,
            unchanged_since: None,
        };
        let bytes = snapshot(SOURCE_MAGIC, &source_head(&index), &index.dirs);
        let back = decode_source_index(&bytes).unwrap();
        assert_eq!(back.cursor, index.cursor);
        assert_eq!(back.root, 99);
        assert_eq!(back.volume_id, "vol");
        assert_eq!(back.dirs[&PathBuf::from("a")][0].size, 12);
        let mut broken = bytes;
        let mid = broken.len() / 2;
        broken[mid] ^= 0xff;
        assert!(decode_source_index(&broken).is_none());
    }

    #[test]
    fn corruption_is_rejected() {
        let mut bytes = encoded(&sample());
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        assert!(decode(&bytes).is_none());
        assert!(decode(&bytes[..4]).is_none());
    }

    #[test]
    fn a_small_change_adds_only_that_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(1000);
        save(&path, &cache).unwrap();
        let full = file_len(&path);

        cache.run_id = "second".into();
        cache
            .dirs
            .get_mut(Path::new("folder5"))
            .unwrap()
            .push(entry("new", 7));
        cache.dirs.remove(Path::new("folder6"));
        save(&path, &cache).unwrap();

        let added = file_len(&path) - full;
        assert!(added < full / 100, "added {added} bytes to {full}");
        let back = load(&path).unwrap();
        assert_eq!(back.run_id, "second");
        assert_eq!(back.dirs.len(), 999);
        assert!(!back.dirs.contains_key(Path::new("folder6")));
        assert_eq!(back.dirs[Path::new("folder5")].len(), 21);
        assert!(same_listing(
            &back.dirs[Path::new("folder7")],
            &cache.dirs[Path::new("folder7")]
        ));
    }

    #[test]
    fn the_same_entries_in_another_order_add_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(10);
        save(&path, &cache).unwrap();
        let before = file_len(&path);
        cache.dirs.get_mut(Path::new("folder1")).unwrap().reverse();
        save(&path, &cache).unwrap();
        // Only the record's frame and head: nothing of the folder itself.
        assert!(file_len(&path) - before < 80);
    }

    #[test]
    fn every_torn_length_gives_an_older_state_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(3);
        save(&path, &cache).unwrap();
        let first_end = file_len(&path) as usize;
        cache.run_id = "second".into();
        cache.dirs.remove(Path::new("folder0"));
        save(&path, &cache).unwrap();
        let whole = fs::read(&path).unwrap();
        assert!(whole.len() > first_end);

        for cut in 0..whole.len() {
            let back = decode(&whole[..cut]);
            if cut < first_end {
                assert!(back.is_none(), "cut at {cut}");
            } else {
                let back = back.unwrap();
                assert_eq!(back.run_id, "first", "cut at {cut}");
                assert_eq!(back.dirs.len(), 3, "cut at {cut}");
            }
        }
        assert_eq!(decode(&whole).unwrap().run_id, "second");
    }

    #[test]
    fn damage_in_an_appended_record_falls_back_to_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(3);
        save(&path, &cache).unwrap();
        let first_end = file_len(&path) as usize;
        cache.run_id = "second".into();
        save(&path, &cache).unwrap();
        let second_end = file_len(&path) as usize;
        cache.run_id = "third".into();
        save(&path, &cache).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        bytes[(first_end + second_end) / 2] ^= 0xff;
        // The third record is whole, but it cannot be applied without the second.
        assert_eq!(decode(&bytes).unwrap().run_id, "first");
    }

    #[test]
    fn a_torn_tail_is_replaced_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(3);
        save(&path, &cache).unwrap();
        let good = fs::read(&path).unwrap();
        let mut torn = good.clone();
        torn.extend([0u8; 20]);
        fs::write(&path, &torn).unwrap();

        cache.run_id = "second".into();
        save(&path, &cache).unwrap();
        assert_eq!(load(&path).unwrap().run_id, "second");
    }

    #[test]
    fn a_grown_log_is_folded_into_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = large(50);
        save(&path, &cache).unwrap();
        let mut size = 0;
        for n in 0..300u64 {
            let listing = cache.dirs.get_mut(Path::new("folder1")).unwrap();
            for e in listing.iter_mut() {
                e.size = n;
            }
            for (i, e) in listing.iter_mut().enumerate() {
                e.name = format!("a-rather-long-file-name-{i}-{n:040}").into();
            }
            cache.run_id = format!("run{n}");
            save(&path, &cache).unwrap();
            size = size.max(file_len(&path));
        }
        let limit = encoded(&cache).len() as u64 + MIN_LOG_BYTES;
        assert!(size <= limit + 4096, "{size} > {limit}");
        let back = load(&path).unwrap();
        assert_eq!(back.run_id, "run299");
        assert!(same_listing(
            &back.dirs[Path::new("folder1")],
            &cache.dirs[Path::new("folder1")]
        ));
    }

    #[test]
    fn an_unchanged_source_adds_a_small_record_and_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.src.bin");
        let mut index = SourceIndex {
            cursor: Cursor {
                journal_id: 7,
                next_usn: 100,
            },
            root: 9,
            volume_id: "volume".into(),
            dirs: large(100).dirs,
            unchanged_since: None,
        };
        save_source_index(&path, &index).unwrap();
        let before = file_len(&path);

        index.unchanged_since = load_source_index(&path).unwrap().unchanged_since;
        index.cursor.next_usn = 200;
        save_source_index(&path, &index).unwrap();
        assert!(file_len(&path) - before < 120);

        let back = load_source_index(&path).unwrap();
        assert_eq!(back.cursor.next_usn, 200);
        assert_eq!(back.dirs.len(), 100);
    }

    #[test]
    fn losing_the_new_position_leaves_the_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.src.bin");
        let mut index = SourceIndex {
            cursor: Cursor {
                journal_id: 7,
                next_usn: 100,
            },
            root: 9,
            volume_id: "volume".into(),
            dirs: sample().dirs,
            unchanged_since: None,
        };
        save_source_index(&path, &index).unwrap();
        let whole = file_len(&path);
        index.unchanged_since = load_source_index(&path).unwrap().unchanged_since;
        index.cursor.next_usn = 200;
        save_source_index(&path, &index).unwrap();

        let bytes = fs::read(&path).unwrap();
        let cut = bytes.len() - 1;
        fs::write(&path, &bytes[..cut]).unwrap();
        let back = load_source_index(&path).unwrap();
        // The journal is read again from here, so nothing is missed.
        assert_eq!(back.cursor.next_usn, 100);
        assert_eq!(back.unchanged_since.unwrap().valid_len, whole);
    }
}
