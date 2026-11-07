//! Types shared by the scanner, planner and executor.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Reserved file in the destination root that ties it to a cache generation.
pub const MARKER_NAME: &str = ".hayakagami-state";
/// Suffix of in-flight copies. Leftovers are removed by the next full scan.
pub const TMP_SUFFIX: &str = ".hkgmtmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symlink or similar that must not be followed.
    Link,
    /// A link that the OS removes as a directory (Windows junction).
    DirLink,
}

impl Kind {
    pub fn is_link(self) -> bool {
        matches!(self, Kind::Link | Kind::DirLink)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    pub size: u64,
    /// Last modification, in 100 ns units since the Unix epoch.
    pub mtime: i64,
    /// Creation time in the same units, or 0 where the listing has none.
    pub created: i64,
    /// Read-only, hidden and system on Windows; the permission bits on Unix.
    pub attrs: u32,
}

/// One source-to-destination folder mirror.
#[derive(Clone, Debug)]
pub struct PairSpec {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub excludes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Ignore the cache and list the destination.
    pub force_full: bool,
    /// Force a full scan when the last one is older than this. 0 disables the cache.
    pub full_check_days: u32,
    /// Use the NTFS change journal to avoid listing unchanged source folders.
    pub use_journal: bool,
    /// Recreate links in the destination, with their targets unchanged.
    pub copy_links: bool,
    /// Reads the journal when this process lacks the rights to.
    pub journal_remote: Option<crate::journal::RemoteHandle>,
    pub scan_threads: Option<usize>,
    pub copy_threads: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            force_full: false,
            full_check_days: 30,
            use_journal: false,
            copy_links: false,
            journal_remote: None,
            scan_threads: None,
            copy_threads: None,
        }
    }
}

#[derive(Debug)]
pub enum MirrorError {
    /// A safety check refused to continue.
    Guard(String),
    Cancelled,
    Fatal(String),
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorError::Guard(m) | MirrorError::Fatal(m) => f.write_str(m),
            MirrorError::Cancelled => f.write_str(crate::i18n::tr("Cancelled", "中止しました")),
        }
    }
}

impl std::error::Error for MirrorError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    Scanning = 1,
    Copying = 2,
    Deleting = 3,
    Metadata = 4,
}

/// Counters read by the UI while a job runs.
#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    pub source_entries: AtomicU64,
    pub dest_entries: AtomicU64,
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub deletes_total: AtomicU64,
    pub deletes_done: AtomicU64,
    pub metadata_total: AtomicU64,
    pub metadata_done: AtomicU64,
    pub errors: AtomicU64,
}

impl Progress {
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Scanning,
            2 => Phase::Copying,
            3 => Phase::Deleting,
            4 => Phase::Metadata,
            _ => Phase::Idle,
        }
    }

    /// Clears the counters so one progress display can follow several pairs.
    pub fn reset(&self) {
        for c in [
            &self.source_entries,
            &self.dest_entries,
            &self.files_total,
            &self.files_done,
            &self.bytes_total,
            &self.bytes_done,
            &self.deletes_total,
            &self.deletes_done,
            &self.metadata_total,
            &self.metadata_done,
            &self.errors,
        ] {
            c.store(0, Ordering::Relaxed);
        }
    }
}

/// Path text for people: hides the extended-length prefix Windows paths carry here.
pub fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    }
}

pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct RunContext {
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<Progress>,
    pub log: LogFn,
}

impl RunContext {
    pub fn new(log: LogFn) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::default()),
            log,
        }
    }

    pub fn silent() -> Self {
        Self::new(Arc::new(|_| {}))
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn log(&self, message: impl Into<String>) {
        (self.log)(message.into());
    }
}

/// Comparison key for names. It follows the common behaviour of each OS: names
/// differing only in case are one file on Windows and macOS, and macOS also
/// treats Unicode normalization forms as equal.
pub fn name_key(name: &OsStr) -> String {
    let text = name.to_string_lossy();
    #[cfg(target_os = "macos")]
    {
        use unicode_normalization::UnicodeNormalization;
        text.nfc().collect::<String>().to_lowercase()
    }
    #[cfg(windows)]
    {
        text.to_lowercase()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        text.into_owned()
    }
}
