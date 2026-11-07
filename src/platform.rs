//! Operating system specifics: bulk listing, copying and disk detection.

use crate::model::Entry;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskKind {
    Ssd,
    /// Rotating disk, or a disk that cannot be identified.
    Hdd,
    Network,
}

impl DiskKind {
    pub fn scan_threads(self) -> usize {
        match self {
            DiskKind::Hdd => 2,
            DiskKind::Ssd | DiskKind::Network => 8,
        }
    }

    pub fn copy_threads(self) -> usize {
        match self {
            DiskKind::Hdd => 1,
            DiskKind::Ssd | DiskKind::Network => 4,
        }
    }
}

/// Converts a name to bytes for the cache file.
pub fn os_to_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
}

pub fn os_from_bytes(bytes: &[u8]) -> Option<OsString> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let (pairs, _) = bytes.as_chunks::<2>();
        let wide: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Some(OsString::from_wide(&wide))
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(OsString::from_vec(bytes.to_vec()))
    }
}

/// Metadata to apply without touching a file's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub created: i64,
    pub mtime: Option<i64>,
    pub attrs: Option<u32>,
}

/// What a link is, exactly as stored: the target is never resolved or rewritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    pub kind: LinkKind,
    pub target: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// A symbolic link. Unix makes no difference between targets.
    File,
    /// A Windows symbolic link to a folder.
    Dir,
    /// A Windows junction.
    Junction,
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(unix)]
pub use unix_impl::*;

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use crate::model::Kind;
    use std::ffi::{c_void, OsStr};
    use std::fs;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Component;
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
        ERROR_REQUEST_ABORTED, FILETIME, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CopyFileExW, CreateFileW, FindClose, FindExInfoBasic, FindExSearchNameMatch,
        FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetFileAttributesW,
        GetVolumeInformationW, GetVolumePathNameW, SetFileAttributesW, SetFileTime,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, FIND_FIRST_EX_LARGE_FETCH, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
        WIN32_FIND_DATAW,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
        FSCTL_SET_REPARSE_POINT, IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const DRIVE_REMOTE: u32 = 4;
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    const NAME_SURROGATE_BIT: u32 = 0x2000_0000;
    /// Read-only, hidden and system. Others describe the disk's copy, such as
    /// archive, which every write sets.
    const COPIED_ATTRIBUTES: u32 = 0x7;
    const PROGRESS_CONTINUE: u32 = 0;
    const PROGRESS_CANCEL: u32 = 1;

    const CLASS_KEY: &str = r"HKCU\Software\Classes\Hikae.Pattern";
    const EXTENSION_KEY: &str = r"HKCU\Software\Classes\.hikae";
    const LEGACY_CLASS_KEY: &str = r"HKCU\Software\Classes\Hayakagami.Pattern";
    const LEGACY_EXTENSION_KEY: &str = r"HKCU\Software\Classes\.hkgm";

    fn run_reg(args: &[&str]) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ))
        }
    }

    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
    }

    /// Opens .hikae and earlier .hkgm files with `exe`, for this user only. No
    /// administrator rights are needed.
    pub fn register_file_type(exe: &Path) -> io::Result<()> {
        let exe = exe.display().to_string();
        let command = format!("\"{exe}\" \"%1\"");
        let icon = format!("{exe},0");
        let command_key = format!(r"{CLASS_KEY}\shell\open\command");
        let icon_key = format!(r"{CLASS_KEY}\DefaultIcon");
        for (key, value) in [
            (EXTENSION_KEY, "Hikae.Pattern"),
            (LEGACY_EXTENSION_KEY, "Hikae.Pattern"),
            (CLASS_KEY, "Hikae pattern"),
            (icon_key.as_str(), icon.as_str()),
            (command_key.as_str(), command.as_str()),
        ] {
            run_reg(&["add", key, "/ve", "/d", value, "/f"])?;
        }
        let _ = run_reg(&["delete", LEGACY_CLASS_KEY, "/f"]);
        notify_shell();
        Ok(())
    }

    /// Moves the link made under the earlier name of this program over to `exe`.
    pub fn migrate_file_type(exe: &Path) -> io::Result<()> {
        if run_reg(&["query", LEGACY_CLASS_KEY]).is_ok() {
            register_file_type(exe)?;
        }
        Ok(())
    }

    /// Removes what `register_file_type` added.
    pub fn unregister_file_type() -> io::Result<()> {
        let first = run_reg(&["delete", EXTENSION_KEY, "/f"]);
        let second = run_reg(&["delete", CLASS_KEY, "/f"]);
        let _ = run_reg(&["delete", LEGACY_EXTENSION_KEY, "/f"]);
        let _ = run_reg(&["delete", LEGACY_CLASS_KEY, "/f"]);
        notify_shell();
        first.and(second)
    }

    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Lists a folder in one call per directory, without opening each file.
    pub fn list_dir(path: &Path) -> io::Result<Vec<Entry>> {
        let pattern = wide(path.join("*").as_os_str());
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                (&mut data as *mut WIN32_FIND_DATAW).cast::<c_void>(),
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return if code == ERROR_FILE_NOT_FOUND {
                Ok(Vec::new())
            } else {
                Err(io::Error::from_raw_os_error(code as i32))
            };
        }
        let mut out = Vec::new();
        loop {
            push_entry(&data, &mut out);
            if unsafe { FindNextFileW(handle, &mut data) } == 0 {
                let code = unsafe { GetLastError() };
                unsafe { FindClose(handle) };
                return if code == ERROR_NO_MORE_FILES {
                    Ok(out)
                } else {
                    Err(io::Error::from_raw_os_error(code as i32))
                };
            }
        }
    }

    /// The modification time and attributes held by a folder itself. What a parent
    /// folder's listing says about it can lag behind.
    pub fn actual_dir_meta(path: &Path) -> Option<(i64, u32)> {
        use std::os::windows::fs::MetadataExt;
        let meta = fs::metadata(path).ok()?;
        Some((
            meta.last_write_time() as i64 - EPOCH_DIFF_100NS,
            meta.file_attributes() & COPIED_ATTRIBUTES,
        ))
    }

    /// A folder's attributes compare as they are.
    pub fn dir_attrs(attrs: u32) -> u32 {
        attrs
    }

    fn to_filetime(time: i64) -> FILETIME {
        let value = time + EPOCH_DIFF_100NS;
        FILETIME {
            dwLowDateTime: value as u32,
            dwHighDateTime: (value >> 32) as u32,
        }
    }

    /// Applies metadata to an existing file or folder without touching contents.
    pub fn set_metadata(path: &Path, meta: &Meta) -> io::Result<()> {
        let name = wide(path.as_os_str());
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_WRITE_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let created = to_filetime(meta.created);
        let modified = meta.mtime.map(to_filetime);
        let modified_ptr = modified
            .as_ref()
            .map_or(std::ptr::null(), |time| time as *const FILETIME);
        let ok = unsafe { SetFileTime(handle, &created, std::ptr::null(), modified_ptr) };
        let timed = if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        unsafe { CloseHandle(handle) };
        timed?;
        let Some(attrs) = meta.attrs else {
            return Ok(());
        };
        let current = unsafe { GetFileAttributesW(name.as_ptr()) };
        if current == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        let wanted = (current & !COPIED_ATTRIBUTES) | (attrs & COPIED_ATTRIBUTES);
        if wanted == current {
            return Ok(());
        }
        if unsafe { SetFileAttributesW(name.as_ptr(), wanted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads a link without following it. Other reparse points are unsupported.
    pub fn read_link(path: &Path) -> io::Result<LinkInfo> {
        const TAG_SYMLINK: u32 = 0xA000_000C;
        const TAG_JUNCTION: u32 = 0xA000_0003;
        let name = wide(path.as_os_str());
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = unsafe {
            FindFirstFileExW(
                name.as_ptr(),
                FindExInfoBasic,
                (&mut data as *mut WIN32_FIND_DATAW).cast::<c_void>(),
                FindExSearchNameMatch,
                std::ptr::null(),
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        unsafe { FindClose(handle) };
        if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let kind = match (data.dwReserved0, is_dir) {
            (TAG_JUNCTION, _) => LinkKind::Junction,
            (TAG_SYMLINK, true) => LinkKind::Dir,
            (TAG_SYMLINK, false) => LinkKind::File,
            _ => return Err(io::ErrorKind::Unsupported.into()),
        };
        Ok(LinkInfo {
            kind,
            target: fs::read_link(path)?,
        })
    }

    /// Creates a link at `path`, which must not exist.
    pub fn create_link(path: &Path, link: &LinkInfo) -> io::Result<()> {
        match link.kind {
            LinkKind::File => std::os::windows::fs::symlink_file(&link.target, path),
            LinkKind::Dir => std::os::windows::fs::symlink_dir(&link.target, path),
            LinkKind::Junction => create_junction(path, &link.target),
        }
    }

    /// The bytes of a mount point reparse buffer for a target.
    fn junction_buffer(target: &Path) -> io::Result<Vec<u8>> {
        const TAG_JUNCTION: u32 = 0xA000_0003;
        const HEADER_AFTER_LENGTHS: u16 = 8;
        let text = target.to_string_lossy();
        let bare = text
            .strip_prefix(r"\\?\")
            .or_else(|| text.strip_prefix(r"\??\"))
            .unwrap_or(&text);
        let print: Vec<u16> = OsStr::new(bare).encode_wide().collect();
        let mut substitute: Vec<u16> = OsStr::new(r"\??\").encode_wide().collect();
        substitute.extend(&print);
        let sub_bytes = u16::try_from(substitute.len() * 2)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let print_bytes = u16::try_from(print.len() * 2)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let data_length = sub_bytes
            .checked_add(print_bytes)
            .and_then(|n| n.checked_add(HEADER_AFTER_LENGTHS + 4))
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut out = Vec::with_capacity(usize::from(data_length) + 8);
        out.extend(TAG_JUNCTION.to_le_bytes());
        out.extend(data_length.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(sub_bytes.to_le_bytes());
        out.extend((sub_bytes + 2).to_le_bytes());
        out.extend(print_bytes.to_le_bytes());
        for unit in substitute
            .iter()
            .chain(&[0u16])
            .chain(&print)
            .chain(&[0u16])
        {
            out.extend(unit.to_le_bytes());
        }
        Ok(out)
    }

    fn create_junction(path: &Path, target: &Path) -> io::Result<()> {
        let buffer = junction_buffer(target)?;
        fs::create_dir(path)?;
        let set = || -> io::Result<()> {
            let name = wide(path.as_os_str());
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut got = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_SET_REPARSE_POINT,
                    buffer.as_ptr().cast(),
                    buffer.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            let result = if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
            unsafe { CloseHandle(handle) };
            result
        };
        let result = set();
        if result.is_err() {
            // The empty folder is ours, made a moment ago.
            let _ = fs::remove_dir(path);
        }
        result
    }

    /// True when this account or file system cannot make links at all, as opposed
    /// to one link failing: no privilege, or no support on the destination.
    pub fn link_unavailable(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(1 | 50 | 1314 | 4390))
            || error.kind() == io::ErrorKind::Unsupported
    }

    fn push_entry(data: &WIN32_FIND_DATAW, out: &mut Vec<Entry>) {
        let len = data
            .cFileName
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        const DOT: u16 = b'.' as u16;
        if name == [DOT] || name == [DOT, DOT] {
            return;
        }
        let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        // Only name surrogates (symlinks, junctions) are links. Other reparse
        // tags, such as cloud placeholders and dedup, behave as ordinary items.
        let is_link = data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && data.dwReserved0 & NAME_SURROGATE_BIT != 0;
        let kind = match (is_link, is_dir) {
            (true, true) => Kind::DirLink,
            (true, false) => Kind::Link,
            (false, true) => Kind::Dir,
            (false, false) => Kind::File,
        };
        let filetime = ((data.ftLastWriteTime.dwHighDateTime as i64) << 32)
            | data.ftLastWriteTime.dwLowDateTime as i64;
        let created = ((data.ftCreationTime.dwHighDateTime as i64) << 32)
            | data.ftCreationTime.dwLowDateTime as i64;
        out.push(Entry {
            name: OsString::from_wide(name),
            kind,
            size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
            mtime: filetime - EPOCH_DIFF_100NS,
            created: created - EPOCH_DIFF_100NS,
            attrs: data.dwFileAttributes & COPIED_ATTRIBUTES,
        });
    }

    struct CopyState<'a> {
        cancel: &'a AtomicBool,
        on_bytes: &'a mut dyn FnMut(u64),
        last: u64,
    }

    unsafe extern "system" fn copy_progress(
        _total: i64,
        transferred: i64,
        _stream_size: i64,
        _stream_transferred: i64,
        _stream_number: u32,
        _reason: u32,
        _source: HANDLE,
        _dest: HANDLE,
        data: *const c_void,
    ) -> u32 {
        let state = &mut *(data as *mut CopyState<'_>);
        let now = transferred as u64;
        if now > state.last {
            (state.on_bytes)(now - state.last);
            state.last = now;
        }
        if state.cancel.load(Ordering::Relaxed) {
            PROGRESS_CANCEL
        } else {
            PROGRESS_CONTINUE
        }
    }

    /// Copies contents, attributes, alternate streams and the modification time.
    /// Forces a finished copy onto the disk. Its size and date look right from the
    /// moment it is renamed, so a later run could not tell if the data were lost.
    fn flush_copy(path: &Path) -> io::Result<()> {
        let mut perms = fs::metadata(path)?.permissions();
        let readonly = perms.readonly();
        if readonly {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(path, perms.clone())?;
        }
        let flushed = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.sync_all());
        if readonly {
            perms.set_readonly(true);
            fs::set_permissions(path, perms)?;
        }
        match flushed {
            // Some network file systems have no flush to offer.
            Err(e) if matches!(e.raw_os_error(), Some(1 | 50)) => Ok(()),
            other => other,
        }
    }

    pub fn copy_file(
        src: &Path,
        dst: &Path,
        cancel: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
    ) -> io::Result<()> {
        let src_w = wide(src.as_os_str());
        let dst_w = wide(dst.as_os_str());
        let mut state = CopyState {
            cancel,
            on_bytes,
            last: 0,
        };
        let ok = unsafe {
            CopyFileExW(
                src_w.as_ptr(),
                dst_w.as_ptr(),
                Some(copy_progress),
                (&mut state as *mut CopyState<'_>).cast::<c_void>(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 {
            return flush_copy(dst);
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_REQUEST_ABORTED {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    fn volume_root(path: &Path) -> Option<Vec<u16>> {
        let input = wide(path.as_os_str());
        let mut buf = [0u16; 1024];
        let ok = unsafe { GetVolumePathNameW(input.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if ok == 0 {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0)?;
        let mut root = buf[..len].to_vec();
        root.push(0);
        Some(root)
    }

    /// Returns the volume serial and the file system name.
    pub fn volume_info(path: &Path) -> Option<(String, String)> {
        let root = volume_root(path)?;
        let mut serial = 0u32;
        let mut fs = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let len = fs.iter().position(|c| *c == 0).unwrap_or(fs.len());
        Some((
            format!("{serial:08x}"),
            String::from_utf16_lossy(&fs[..len]),
        ))
    }

    /// The volume label and serial number. The serial changes when a disk is formatted.
    pub fn disk_identity(path: &Path) -> Option<crate::model::DiskIdentity> {
        let root = volume_root(path)?;
        let mut serial = 0u32;
        let mut label = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return None;
        }
        let len = label.iter().position(|c| *c == 0).unwrap_or(label.len());
        let name = String::from_utf16_lossy(&label[..len]);
        let id = if serial == 0 {
            String::new()
        } else {
            format!("{serial:08X}")
        };
        (!name.is_empty() || !id.is_empty()).then_some(crate::model::DiskIdentity { name, id })
    }

    /// Time difference accepted as equal. FAT family disks round timestamps.
    pub fn mtime_tolerance(path: &Path) -> i64 {
        match volume_info(path) {
            Some((_, fs)) if fs.eq_ignore_ascii_case("NTFS") || fs.eq_ignore_ascii_case("ReFS") => {
                0
            }
            _ => 20_000_000,
        }
    }

    pub fn free_space(path: &Path) -> Option<u64> {
        let input = wide(path.as_os_str());
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                input.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(free)
    }

    pub fn disk_kind(path: &Path) -> DiskKind {
        let Some(root) = volume_root(path) else {
            return DiskKind::Hdd;
        };
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
            return DiskKind::Network;
        }
        let letter = path.components().find_map(|c| match c {
            Component::Prefix(p) => match p.kind() {
                std::path::Prefix::VerbatimDisk(l) | std::path::Prefix::Disk(l) => Some(l as char),
                std::path::Prefix::VerbatimUNC(..) | std::path::Prefix::UNC(..) => Some('\0'),
                _ => None,
            },
            _ => None,
        });
        match letter {
            Some('\0') => DiskKind::Network,
            Some(l) => seek_penalty(l).unwrap_or(DiskKind::Hdd),
            None => DiskKind::Hdd,
        }
    }

    fn seek_penalty(letter: char) -> Option<DiskKind> {
        let device = wide(OsStr::new(&format!("\\\\.\\{letter}:")));
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
        query.PropertyId = StorageDeviceSeekPenaltyProperty;
        query.QueryType = PropertyStandardQuery;
        let mut result: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&query as *const STORAGE_PROPERTY_QUERY).cast::<c_void>(),
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                (&mut result as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR).cast::<c_void>(),
                std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        Some(if result.IncursSeekPenalty {
            DiskKind::Hdd
        } else {
            DiskKind::Ssd
        })
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use crate::model::Kind;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirEntryExt, MetadataExt};
    use std::sync::atomic::Ordering;

    /// Lists a folder. Reading metadata in inode order reduces disk seeks.
    pub fn list_dir(path: &Path) -> io::Result<Vec<Entry>> {
        let mut names: Vec<(u64, std::path::PathBuf, OsString)> = Vec::new();
        for item in fs::read_dir(path)? {
            let item = item?;
            names.push((item.ino(), item.path(), item.file_name()));
        }
        names.sort_unstable_by_key(|n| n.0);
        let mut out = Vec::with_capacity(names.len());
        for (_, full, name) in names {
            let meta = match fs::symlink_metadata(&full) {
                Ok(m) => m,
                // Removed between the listing and the stat.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let kind = if meta.file_type().is_symlink() {
                Kind::Link
            } else if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File
            } else {
                // Devices, sockets and pipes are not copied, and never followed.
                Kind::Link
            };
            out.push(Entry {
                name,
                kind,
                size: meta.len(),
                mtime: meta.mtime() * 10_000_000 + meta.mtime_nsec() / 100,
                created: 0,
                attrs: meta.mode() & 0o7777,
            });
        }
        Ok(out)
    }

    /// Copies contents, permissions and the modification time.
    pub fn copy_file(
        src: &Path,
        dst: &Path,
        cancel: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
    ) -> io::Result<()> {
        let mut input = fs::File::open(src)?;
        let meta = input.metadata()?;
        let mut output = fs::File::create_new(dst)?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            on_bytes(n as u64);
        }
        output.set_permissions(meta.permissions())?;
        output.set_modified(meta.modified()?)?;
        // Its size and date look right from the moment it is renamed, so a later
        // run could not tell if the data were lost.
        output.sync_all()
    }

    pub fn volume_info(path: &Path) -> Option<(String, String)> {
        let meta = fs::metadata(path).ok()?;
        Some((format!("{:x}", meta.dev()), String::new()))
    }

    /// The name of the mount point, plus the file system UUID where the system lists
    /// one (Linux). The device number is not used, as it changes between connections.
    pub fn disk_identity(path: &Path) -> Option<crate::model::DiskIdentity> {
        let path = path.canonicalize().ok()?;
        let device = fs::metadata(&path).ok()?.dev();
        let mount = path
            .ancestors()
            .take_while(|p| fs::metadata(p).is_ok_and(|m| m.dev() == device))
            .last()?;
        let name = mount.file_name().map_or_else(
            || mount.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let id = fs::read_dir("/dev/disk/by-uuid")
            .into_iter()
            .flatten()
            .flatten()
            .find(|link| fs::metadata(link.path()).is_ok_and(|m| m.rdev() == device))
            .map_or(String::new(), |link| {
                link.file_name().to_string_lossy().into_owned()
            });
        Some(crate::model::DiskIdentity { name, id })
    }

    /// The file system type is not checked here, so the coarsest common
    /// timestamp resolution (2 seconds, FAT) is accepted.
    pub fn mtime_tolerance(_path: &Path) -> i64 {
        20_000_000
    }

    pub fn free_space(path: &Path) -> Option<u64> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(bytes.as_ptr().cast(), &mut stat) };
        (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    /// The modification time and permission bits held by a folder itself.
    pub fn actual_dir_meta(path: &Path) -> Option<(i64, u32)> {
        let meta = fs::metadata(path).ok()?;
        Some((
            meta.mtime() * 10_000_000 + meta.mtime_nsec() / 100,
            meta.mode() & 0o7777,
        ))
    }

    /// The owner always keeps full access to a destination folder, or a later
    /// run could not write into it.
    pub fn dir_attrs(attrs: u32) -> u32 {
        attrs | 0o700
    }

    /// Applies metadata to an existing file or folder without touching contents.
    pub fn set_metadata(path: &Path, meta: &Meta) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, SystemTime};
        if let Some(mtime) = meta.mtime {
            let offset = Duration::from_nanos(mtime.unsigned_abs().saturating_mul(100));
            let time = if mtime >= 0 {
                SystemTime::UNIX_EPOCH + offset
            } else {
                SystemTime::UNIX_EPOCH - offset
            };
            fs::File::open(path)?.set_modified(time)?;
        }
        if let Some(attrs) = meta.attrs {
            fs::set_permissions(path, fs::Permissions::from_mode(attrs))?;
        }
        Ok(())
    }

    /// Reads a link without following it. Devices and the like are unsupported.
    pub fn read_link(path: &Path) -> io::Result<LinkInfo> {
        if !fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        Ok(LinkInfo {
            kind: LinkKind::File,
            target: fs::read_link(path)?,
        })
    }

    /// Creates a link at `path`, which must not exist.
    pub fn create_link(path: &Path, link: &LinkInfo) -> io::Result<()> {
        std::os::unix::fs::symlink(&link.target, path)
    }

    /// True when the destination file system cannot hold links at all.
    pub fn link_unavailable(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::EPERM | libc::ENOTSUP | libc::ENOSYS)
        ) || error.kind() == io::ErrorKind::Unsupported
    }

    /// File type registration is only implemented for Windows.
    pub fn migrate_file_type(_exe: &Path) -> io::Result<()> {
        Ok(())
    }

    pub fn register_file_type(_exe: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn unregister_file_type() -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn disk_kind(path: &Path) -> DiskKind {
        #[cfg(target_os = "linux")]
        {
            let Ok(meta) = fs::metadata(path) else {
                return DiskKind::Hdd;
            };
            let dev = meta.dev();
            let (major, minor) = (libc::major(dev), libc::minor(dev));
            let base = format!("/sys/dev/block/{major}:{minor}");
            for candidate in ["queue/rotational", "../queue/rotational"] {
                if let Ok(text) = fs::read_to_string(format!("{base}/{candidate}")) {
                    return if text.trim() == "0" {
                        DiskKind::Ssd
                    } else {
                        DiskKind::Hdd
                    };
                }
            }
            DiskKind::Hdd
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            DiskKind::Hdd
        }
    }
}
