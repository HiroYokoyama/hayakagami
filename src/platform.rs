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
        if bytes.len() % 2 != 0 {
            return None;
        }
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(OsString::from_wide(&wide))
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(OsString::from_vec(bytes.to_vec()))
    }
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
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Component;
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
        ERROR_REQUEST_ABORTED, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CopyFileExW, CreateFileW, FindClose, FindExInfoBasic, FindExSearchNameMatch,
        FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
        GetVolumePathNameW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, OPEN_EXISTING,
        WIN32_FIND_DATAW,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const DRIVE_REMOTE: u32 = 4;
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    const NAME_SURROGATE_BIT: u32 = 0x2000_0000;
    const PROGRESS_CONTINUE: u32 = 0;
    const PROGRESS_CANCEL: u32 = 1;

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
        out.push(Entry {
            name: OsString::from_wide(name),
            kind,
            size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
            mtime: filetime - EPOCH_DIFF_100NS,
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
            return Ok(());
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
        Ok(())
    }

    pub fn volume_info(path: &Path) -> Option<(String, String)> {
        let meta = fs::metadata(path).ok()?;
        Some((format!("{:x}", meta.dev()), String::new()))
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
