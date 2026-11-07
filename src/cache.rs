//! Remembered destination listings.
//!
//! A cache generation is trusted only while the marker in the destination root
//! holds the same run id. A run overwrites the marker when it starts and writes
//! the new id only after a clean finish, so an interrupted run, another machine
//! or a replaced disk all lead to a full scan.

use crate::model::{Entry, Kind, MARKER_NAME};
use crate::platform;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"HKGMCCH1";

pub type DirMap = HashMap<PathBuf, Vec<Entry>>;

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

fn encode(cache: &CacheFile) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + cache.dirs.len() * 64);
    out.extend(MAGIC);
    put_bytes(&mut out, cache.run_id.as_bytes());
    put_bytes(&mut out, cache.volume_id.as_bytes());
    out.extend(cache.last_full.to_le_bytes());
    out.extend((cache.dirs.len() as u64).to_le_bytes());
    for (rel, entries) in &cache.dirs {
        put_bytes(&mut out, &platform::os_to_bytes(rel.as_os_str()));
        out.extend((entries.len() as u64).to_le_bytes());
        for e in entries {
            put_bytes(&mut out, &platform::os_to_bytes(&e.name));
            out.push(match e.kind {
                Kind::File => 0,
                Kind::Dir => 1,
                Kind::Link => 2,
                Kind::DirLink => 3,
            });
            out.extend(e.size.to_le_bytes());
            out.extend(e.mtime.to_le_bytes());
        }
    }
    let hash = fnv1a(&out);
    out.extend(hash.to_le_bytes());
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

fn decode(data: &[u8]) -> Option<CacheFile> {
    let body_len = data.len().checked_sub(8)?;
    let stored = u64::from_le_bytes(data[body_len..].try_into().ok()?);
    if fnv1a(&data[..body_len]) != stored {
        return None;
    }
    let mut r = Reader {
        data: &data[..body_len],
        pos: 0,
    };
    if r.take(8)? != MAGIC {
        return None;
    }
    let run_id = r.text()?;
    let volume_id = r.text()?;
    let last_full = r.u64()?;
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
            entries.push(Entry {
                name,
                kind,
                size,
                mtime,
            });
        }
        dirs.insert(rel, entries);
    }
    Some(CacheFile {
        run_id,
        volume_id,
        last_full,
        dirs,
    })
}

pub fn load(path: &Path) -> Option<CacheFile> {
    decode(&fs::read(path).ok()?)
}

pub fn save(path: &Path, cache: &CacheFile) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&encode(cache))?;
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
            }],
        );
        CacheFile {
            run_id: "r".into(),
            volume_id: "v".into(),
            last_full: 9,
            dirs,
        }
    }

    #[test]
    fn round_trip() {
        let back = decode(&encode(&sample())).unwrap();
        assert_eq!(back.run_id, "r");
        assert_eq!(back.last_full, 9);
        let e = &back.dirs[&PathBuf::from("a")][0];
        assert_eq!(e.name, "日本語.txt");
        assert_eq!(e.mtime, -5);
        assert_eq!(e.kind, Kind::File);
    }

    #[test]
    fn corruption_is_rejected() {
        let mut bytes = encode(&sample());
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        assert!(decode(&bytes).is_none());
        assert!(decode(&bytes[..4]).is_none());
    }
}
