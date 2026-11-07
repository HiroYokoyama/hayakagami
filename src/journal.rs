//! Finds the source folders that changed, using the NTFS change journal.
//!
//! Each run saves the source listings together with the journal position taken
//! before they were read. The next run reads the records written since then and
//! lists only the folders they name; every other listing comes from the saved
//! copy. Anything that casts doubt on the records leads to an ordinary scan.

use crate::cache::{self, DirMap};
use crate::model::name_key;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const REASON_FILE_CREATE: u32 = 0x0000_0100;
const REASON_FILE_DELETE: u32 = 0x0000_0200;
const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
const ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// More changed folders than this make resolving them slower than a plain scan.
pub const MAX_CHANGED_FOLDERS: usize = 20_000;

/// A journal and a position in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// File references named by journal records.
#[derive(Debug, Default)]
pub struct Changes {
    /// Folders whose direct contents changed.
    pub listed: HashSet<u64>,
    /// Folders created, moved or renamed. Nothing saved beneath them can be trusted.
    pub trees: HashSet<u64>,
    /// Folders that were deleted, so they cannot be opened to find their path.
    pub gone: HashSet<u64>,
}

impl Changes {
    pub fn add(&mut self, file: u64, parent: u64, reason: u32, attributes: u32) {
        self.listed.insert(parent);
        if attributes & ATTRIBUTE_DIRECTORY == 0 {
            return;
        }
        if reason & REASON_FILE_DELETE != 0 {
            self.gone.insert(file);
        }
        let moved = REASON_FILE_CREATE
            | REASON_RENAME_OLD_NAME
            | REASON_RENAME_NEW_NAME
            | REASON_REPARSE_POINT_CHANGE;
        if reason & moved != 0 {
            self.trees.insert(file);
        }
    }

    pub fn folder_count(&self) -> usize {
        self.listed.len() + self.trees.len()
    }

    /// Parses version 2 records. Other versions and truncated data are errors,
    /// because a record that is skipped would be a change that goes unnoticed.
    pub fn parse(&mut self, records: &[u8]) -> Result<(), &'static str> {
        const HEADER: usize = 60;
        let mut pos = 0;
        while pos < records.len() {
            let u16_at = |at: usize| u16::from_le_bytes([records[at], records[at + 1]]);
            let u32_at = |at: usize| {
                u32::from_le_bytes([
                    records[at],
                    records[at + 1],
                    records[at + 2],
                    records[at + 3],
                ])
            };
            let u64_at = |at: usize| {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&records[at..at + 8]);
                u64::from_le_bytes(bytes)
            };
            if records.len() - pos < HEADER {
                return Err("truncated record");
            }
            let length = u32_at(pos) as usize;
            if length < HEADER || length > records.len() - pos {
                return Err("bad record length");
            }
            if u16_at(pos + 4) != 2 {
                return Err("unknown record version");
            }
            self.add(
                u64_at(pos + 8),
                u64_at(pos + 16),
                u32_at(pos + 40),
                u32_at(pos + 52),
            );
            pos += length;
        }
        Ok(())
    }
}

/// Windows errors meaning a file reference names nothing that exists now:
/// file not found, path not found, and the invalid parameter that opening a
/// deleted reference returns.
pub fn no_such_folder(code: i32) -> bool {
    matches!(code, 2 | 3 | 87)
}

/// Comparison key of a relative folder path.
pub fn rel_key(rel: &Path) -> String {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name_key(name)),
            _ => None,
        })
        .collect();
    parts.join("/")
}

/// Drops the saved listings that the changes make unreliable. Folders missing
/// from the map are listed from disk by the walker.
pub fn forget_changed(saved: &mut DirMap, listed: &HashSet<String>, trees: &HashSet<String>) {
    // A folder's own date is held by its parent's listing, so that goes as well.
    let parents: HashSet<&str> = listed
        .iter()
        .chain(trees)
        .filter(|key| !key.is_empty())
        .map(|key| key.rfind('/').map_or("", |end| &key[..end]))
        .collect();
    saved.retain(|rel, _| {
        let key = rel_key(rel);
        if listed.contains(&key)
            || trees.contains(&key)
            || parents.contains(key.as_str())
            || trees.contains("")
        {
            return false;
        }
        !key.match_indices('/')
            .any(|(end, _)| trees.contains(&key[..end]))
    });
}

/// Key of `path` relative to `source`, or None for a folder outside it.
pub fn relative_key(path: &Path, source: &Path) -> Option<String> {
    let path = rel_key(path);
    let source = rel_key(source);
    if source.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(&source)?;
    if rest.is_empty() {
        Some(String::new())
    } else {
        rest.strip_prefix('/').map(str::to_owned)
    }
}

/// Source folders named by the journal, as keys relative to the source.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Keys {
    pub listed: HashSet<String>,
    pub trees: HashSet<String>,
}

/// Why the journal could not be used.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadFailure {
    pub reason: String,
    /// Administrator rights alone would have helped.
    pub denied: bool,
}

/// Reads the journal with rights this process does not have.
pub trait RemoteJournal: Send + Sync {
    fn read(&self, source: &Path, from: Cursor, until: i64) -> Result<Keys, ReadFailure>;
}

/// A shareable reader, so scan options stay simple to clone and print.
#[derive(Clone)]
pub struct RemoteHandle(pub Arc<dyn RemoteJournal>);

impl std::fmt::Debug for RemoteHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteHandle")
    }
}

/// Saved source listings and the journal position they belong to.
pub struct SourceIndex {
    pub cursor: Cursor,
    /// Identity of the source folder, so a swapped ancestor is noticed.
    pub root: u64,
    pub volume_id: String,
    pub dirs: DirMap,
}

/// Where the saved source listings for a pair are kept.
pub fn index_path(cache_file: &Path) -> PathBuf {
    cache_file.with_extension("src.bin")
}

/// Removes saved source listings that are no longer used.
pub fn discard_index(cache_file: &Path) {
    cache::discard(&index_path(cache_file));
}

/// The source side of a scan.
pub struct Prepared {
    pub saved: Option<DirMap>,
    /// Number of folders named by the journal when `saved` is set, otherwise the
    /// reason for reading the source from disk.
    pub outcome: Result<u64, String>,
    /// Set when the journal was unreadable only for lack of administrator rights.
    pub denied: bool,
    /// What to save after this scan.
    pub next: Option<(Cursor, u64, String)>,
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub fn needs_elevation(_source: &Path) -> bool {
    false
}

#[cfg(not(windows))]
pub fn prepare(_source: &Path, _index: &Path, _remote: Option<&RemoteHandle>) -> Prepared {
    use crate::i18n::tr;
    Prepared {
        saved: None,
        outcome: Err(tr(
            "the change journal is available only on Windows",
            "変更ジャーナルはWindowsでのみ使えます",
        )
        .into()),
        denied: false,
        next: None,
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use crate::i18n::tr;
    use crate::platform;
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OpenFileById,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const ACCESS_DENIED: i32 = 5;
    const READ_BUFFER: usize = 1 << 20;

    #[repr(C)]
    #[derive(Default)]
    struct JournalData {
        id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    #[repr(C)]
    struct ReadRequest {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        journal_id: u64,
        min_major: u16,
        max_major: u16,
    }

    /// The u64 buffer keeps records at their required alignment.
    fn aligned_buffer() -> Vec<u64> {
        vec![0u64; READ_BUFFER / 8]
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    fn open(path: &std::ffi::OsStr, access: u32) -> io::Result<Handle> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(handle))
        }
    }

    fn file_reference(handle: &Handle) -> io::Result<u64> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
    }

    fn query(handle: &Handle) -> io::Result<JournalData> {
        let mut data = JournalData::default();
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&mut data as *mut JournalData).cast(),
                std::mem::size_of::<JournalData>() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(data)
    }

    /// Path of the volume itself, such as `\\?\Volume{...}`.
    fn volume_device(path: &Path) -> io::Result<OsString> {
        let input = wide(path.as_os_str());
        let mut root = [0u16; 1024];
        let ok =
            unsafe { GetVolumePathNameW(input.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0u16; 128];
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        let text = &name[..len];
        let text = text.strip_suffix(&[u16::from(b'\\')]).unwrap_or(text);
        Ok(OsString::from_wide(text))
    }

    /// Collects the records from `from` up to `until`.
    fn read_records(volume: &Handle, from: Cursor, until: i64) -> io::Result<Changes> {
        let mut changes = Changes::default();
        let mut buffer = aligned_buffer();
        let mut start = from.next_usn;
        while start < until {
            let request = ReadRequest {
                start_usn: start,
                reason_mask: u32::MAX,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                journal_id: from.journal_id,
                min_major: 2,
                max_major: 2,
            };
            let mut got = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    volume.0,
                    FSCTL_READ_USN_JOURNAL,
                    (&request as *const ReadRequest).cast(),
                    std::mem::size_of::<ReadRequest>() as u32,
                    buffer.as_mut_ptr().cast(),
                    READ_BUFFER as u32,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let got = got as usize;
            if got < 8 {
                return Err(io::Error::other("short journal read"));
            }
            let next = buffer[0] as i64;
            // The buffer outlives this slice, and u8 has no alignment needs.
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), got) };
            changes.parse(&bytes[8..]).map_err(io::Error::other)?;
            if next <= start {
                break;
            }
            start = next;
            if changes.folder_count() > MAX_CHANGED_FOLDERS {
                break;
            }
        }
        Ok(changes)
    }

    fn final_path(root: &Handle, reference: u64) -> io::Result<PathBuf> {
        let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
        descriptor.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
        descriptor.Type = 0;
        descriptor.Anonymous.FileId = reference as i64;
        let handle = unsafe {
            OpenFileById(
                root.0,
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        let mut buf = vec![0u16; 32_768];
        let len =
            unsafe { GetFinalPathNameByHandleW(handle.0, buf.as_mut_ptr(), buf.len() as u32, 0) }
                as usize;
        if len == 0 || len >= buf.len() {
            return Err(io::Error::last_os_error());
        }
        Ok(PathBuf::from(OsString::from_wide(&buf[..len])))
    }

    /// Turns references into keys below `source`. A folder that cannot be found
    /// and has no deletion record could be anywhere, so it stops the shortcut.
    fn resolve(
        root: &Handle,
        source: &Path,
        references: &HashSet<u64>,
        gone: &HashSet<u64>,
    ) -> Result<HashSet<String>, ReadFailure> {
        let mut keys = HashSet::new();
        for reference in references {
            if gone.contains(reference) {
                continue;
            }
            match final_path(root, *reference) {
                Ok(path) => {
                    if let Some(key) = relative_key(&path, source) {
                        keys.insert(key);
                    }
                }
                // Deleted after the records were read. If it was in the source, its
                // deletion names its parent next run. Its saved listing cannot hide
                // a file that still exists, because nothing in it exists any more.
                Err(e) if e.raw_os_error().is_some_and(no_such_folder) => {}
                Err(e) => {
                    return Err(ReadFailure {
                        reason: crate::trf!(
                            "a changed folder could not be located: {e}",
                            "変更されたフォルダを特定できません: {e}"
                        ),
                        denied: e.raw_os_error() == Some(ACCESS_DENIED),
                    });
                }
            }
        }
        Ok(keys)
    }

    /// Reads the records from `from` to `until` and finds the folders they name
    /// below `source`. This is the part that may need administrator rights.
    pub fn read_changes(source: &Path, from: Cursor, until: i64) -> Result<Keys, ReadFailure> {
        let denied = |e: &io::Error| e.raw_os_error() == Some(ACCESS_DENIED);
        let failure = |e: io::Error| {
            if denied(&e) {
                ReadFailure {
                    reason: tr(
                        "reading the change journal needs administrator rights",
                        "変更ジャーナルの読み取りには管理者権限が必要です",
                    )
                    .into(),
                    denied: true,
                }
            } else {
                ReadFailure {
                    reason: crate::trf!(
                        "the change journal could not be read: {e}",
                        "変更ジャーナルを読み取れません: {e}"
                    ),
                    denied: false,
                }
            }
        };
        let volume = volume_device(source)
            .and_then(|name| open(&name, GENERIC_READ))
            .map_err(failure)?;
        let changes = read_records(&volume, from, until).map_err(failure)?;
        if changes.folder_count() > MAX_CHANGED_FOLDERS {
            return Err(ReadFailure {
                reason: tr(
                    "too many folders changed on this volume",
                    "このボリュームで変更されたフォルダが多すぎます",
                )
                .into(),
                denied: false,
            });
        }
        let root = open(source.as_os_str(), FILE_READ_ATTRIBUTES).map_err(failure)?;
        Ok(Keys {
            listed: resolve(&root, source, &changes.listed, &changes.gone)?,
            trees: resolve(&root, source, &changes.trees, &changes.gone)?,
        })
    }

    /// True when reading this source's journal needs rights this process lacks.
    pub fn needs_elevation(source: &Path) -> bool {
        let Some((_, fs_name)) = platform::volume_info(source) else {
            return false;
        };
        if fs_name != "NTFS" {
            return false;
        }
        let has_journal = open(source.as_os_str(), FILE_READ_ATTRIBUTES)
            .and_then(|root| query(&root))
            .is_ok();
        has_journal
            && matches!(
                volume_device(source).and_then(|name| open(&name, GENERIC_READ)),
                Err(e) if e.raw_os_error() == Some(ACCESS_DENIED)
            )
    }

    fn plain(reason: &str) -> Prepared {
        Prepared {
            saved: None,
            outcome: Err(reason.to_owned()),
            denied: false,
            next: None,
        }
    }

    /// Reads the journal for `source`. `index` is the saved listings file.
    pub fn prepare(source: &Path, index: &Path, remote: Option<&RemoteHandle>) -> Prepared {
        let Some((volume_id, fs_name)) = platform::volume_info(source) else {
            return plain(tr(
                "the source volume could not be identified",
                "元フォルダのボリュームを特定できません",
            ));
        };
        if fs_name != "NTFS" {
            return plain(tr(
                "the source is not on an NTFS volume",
                "元フォルダがNTFSではありません",
            ));
        }
        let Ok(root) = open(source.as_os_str(), FILE_READ_ATTRIBUTES) else {
            return plain(tr(
                "the source folder could not be opened",
                "元フォルダを開けません",
            ));
        };
        let (Ok(data), Ok(root_id)) = (query(&root), file_reference(&root)) else {
            return plain(tr(
                "the source volume has no change journal",
                "元フォルダのボリュームに変更ジャーナルがありません",
            ));
        };
        // Taken before anything is listed, so a change made during the scan is
        // read again by the next run instead of being lost.
        let current = Cursor {
            journal_id: data.id,
            next_usn: data.next_usn,
        };
        let next = Some((current, root_id, volume_id.clone()));
        let fail = |reason: String, denied: bool| Prepared {
            saved: None,
            outcome: Err(reason),
            denied,
            next: next.clone(),
        };

        let Some(saved) = cache::load_source_index(index) else {
            return fail(
                tr(
                    "no saved source listing yet",
                    "元フォルダの記録がまだありません",
                )
                .into(),
                false,
            );
        };
        if saved.volume_id != volume_id || saved.root != root_id {
            return fail(
                tr("the source folder changed", "元フォルダが前回と異なります").into(),
                false,
            );
        }
        if saved.cursor.journal_id != data.id {
            return fail(
                tr(
                    "the change journal was recreated",
                    "変更ジャーナルが作り直されました",
                )
                .into(),
                false,
            );
        }
        let from = saved.cursor;
        if from.next_usn < data.first_usn
            || from.next_usn < data.lowest_valid_usn
            || from.next_usn > data.next_usn
        {
            return fail(
                tr(
                    "the change journal no longer covers the last run",
                    "変更ジャーナルに前回以降の記録が残っていません",
                )
                .into(),
                false,
            );
        }

        let keys = match read_changes(source, from, data.next_usn) {
            Err(failure) if failure.denied => match remote {
                Some(remote) => remote.0.read(source, from, data.next_usn),
                None => Err(failure),
            },
            other => other,
        };
        let keys = match keys {
            Ok(keys) => keys,
            Err(failure) => return fail(failure.reason, failure.denied),
        };

        let mut dirs = saved.dirs;
        forget_changed(&mut dirs, &keys.listed, &keys.trees);
        Prepared {
            saved: Some(dirs),
            outcome: Ok((keys.listed.len() + keys.trees.len()) as u64),
            denied: false,
            next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entry, Kind};

    fn record(file: u64, parent: u64, reason: u32, attributes: u32) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out[0..4].copy_from_slice(&64u32.to_le_bytes());
        out[4..6].copy_from_slice(&2u16.to_le_bytes());
        out[8..16].copy_from_slice(&file.to_le_bytes());
        out[16..24].copy_from_slice(&parent.to_le_bytes());
        out[40..44].copy_from_slice(&reason.to_le_bytes());
        out[52..56].copy_from_slice(&attributes.to_le_bytes());
        out
    }

    fn saved(paths: &[&str]) -> DirMap {
        paths
            .iter()
            .map(|p| {
                (
                    PathBuf::from(p),
                    vec![Entry {
                        name: "f".into(),
                        kind: Kind::File,
                        size: 1,
                        mtime: 1,
                        created: 0,
                        attrs: 0,
                    }],
                )
            })
            .collect()
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_file_change_marks_only_its_folder() {
        let mut changes = Changes::default();
        changes.parse(&record(5, 9, 0x1, 0x20)).unwrap();
        assert!(changes.listed.contains(&9));
        assert!(changes.trees.is_empty());
    }

    #[test]
    fn a_moved_folder_marks_its_whole_tree() {
        let mut changes = Changes::default();
        changes
            .parse(&record(5, 9, REASON_RENAME_NEW_NAME, ATTRIBUTE_DIRECTORY))
            .unwrap();
        assert!(changes.listed.contains(&9));
        assert!(changes.trees.contains(&5));
    }

    #[test]
    fn a_deleted_folder_is_remembered() {
        let mut changes = Changes::default();
        changes
            .parse(&record(5, 9, REASON_FILE_DELETE, ATTRIBUTE_DIRECTORY))
            .unwrap();
        assert!(changes.gone.contains(&5));
        assert!(changes.listed.contains(&9));
    }

    #[test]
    fn unreadable_records_are_errors() {
        let mut changes = Changes::default();
        let good = record(5, 9, 1, 0);
        assert!(changes.parse(&good[..30]).is_err());
        let mut version = good.clone();
        version[4] = 3;
        assert!(changes.parse(&version).is_err());
        let mut length = good;
        length[0] = 8;
        assert!(changes.parse(&length).is_err());
    }

    #[test]
    fn only_a_missing_folder_is_ignored() {
        assert!(no_such_folder(87));
        assert!(no_such_folder(2));
        assert!(!no_such_folder(5));
        assert!(!no_such_folder(32));
    }

    #[test]
    fn keys_are_relative_to_the_source() {
        let key = |path: &str, source: &str| relative_key(Path::new(path), Path::new(source));
        assert_eq!(key("data/a/b", "data"), Some("a/b".to_owned()));
        assert_eq!(key("data", "data"), Some(String::new()));
        assert_eq!(key("database/a", "data"), None);
        assert_eq!(key("other", "data"), None);
        // A source at the root of a volume has no folder names of its own.
        assert_eq!(key("a/b", ""), Some("a/b".to_owned()));
    }

    #[test]
    fn changed_folders_are_forgotten() {
        let mut dirs = saved(&["", "a", "a/b", "a/b/c", "ab", "d", "d/e"]);
        forget_changed(&mut dirs, &set(&["d"]), &set(&["a/b"]));
        let mut left: Vec<String> = dirs.keys().map(|p| rel_key(p)).collect();
        left.sort();
        // "" and "a" hold the dates of "d" and "a/b". "ab" shares a prefix with "a/b"
        // only as text, and "d/e" is a child of a folder whose own listing changed,
        // which says nothing about its contents.
        assert_eq!(left, ["ab", "d/e"]);
    }

    #[test]
    fn a_change_at_the_root_tree_forgets_everything() {
        let mut dirs = saved(&["", "a", "a/b"]);
        forget_changed(&mut dirs, &set(&[]), &set(&[""]));
        assert!(dirs.is_empty());
    }

    #[test]
    fn a_change_to_the_root_listing_keeps_the_rest() {
        let mut dirs = saved(&["", "a"]);
        forget_changed(&mut dirs, &set(&[""]), &set(&[]));
        assert_eq!(dirs.len(), 1);
        assert!(dirs.contains_key(Path::new("a")));
    }
}
