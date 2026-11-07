//! Metadata kept in step without copying contents, on temporary folders only.

use hayakagami::exec;
use hayakagami::model::{PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, Plan};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    dest: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hkgm-meta-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub").join("file.txt"), "contents").unwrap();
        Self {
            _root: root,
            source,
            dest,
        }
    }

    fn scan(&self, force_full: bool) -> Plan {
        let options = ScanOptions {
            force_full,
            ..ScanOptions::default()
        };
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }

    /// Runs, then checks that the destination really matches by listing it.
    fn run(&self) -> exec::Report {
        let report = exec::execute(self.scan(false), &RunContext::silent(), true).unwrap();
        let again = self.scan(true);
        let names: Vec<_> = again.metadata.iter().map(|m| m.rel.clone()).collect();
        assert!(!again.has_work(), "still differs: {names:?}");
        report
    }
}

/// Opens a file or folder for changing its dates.
fn open_for_times(path: &Path) -> fs::File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(0x0200_0000)
            .open(path)
            .unwrap()
    }
    #[cfg(unix)]
    {
        fs::File::open(path).unwrap()
    }
}

fn set_mtime(path: &Path, seconds_ago: u64) {
    let time = SystemTime::now() - Duration::from_secs(seconds_ago);
    open_for_times(path).set_modified(time).unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn within_tolerance(a: SystemTime, b: SystemTime) -> bool {
    let gap = a
        .duration_since(b)
        .or_else(|_| b.duration_since(a))
        .unwrap();
    gap <= Duration::from_secs(2)
}

#[test]
fn a_folder_date_is_copied_after_its_contents() {
    let f = Fixture::new();
    set_mtime(&f.source.join("sub"), 100_000);
    f.run();
    assert!(within_tolerance(
        mtime(&f.source.join("sub")),
        mtime(&f.dest.join("sub"))
    ));
}

/// Replacing a file inside a folder changes the destination folder's date, while
/// an edit in place leaves the source folder's date alone.
#[test]
fn a_folder_date_survives_writing_into_it() {
    let f = Fixture::new();
    f.run();
    let folder = f.source.join("sub");
    fs::write(folder.join("file.txt"), "edited, longer").unwrap();
    set_mtime(&folder, 100_000);
    f.run();
    assert!(within_tolerance(mtime(&folder), mtime(&f.dest.join("sub"))));
}

#[test]
fn a_folder_date_change_alone_copies_nothing() {
    let f = Fixture::new();
    f.run();
    set_mtime(&f.source.join("sub"), 100_000);
    let plan = f.scan(false);
    assert!(plan.copies.is_empty());
    assert_eq!(plan.metadata.len(), 1);
    assert!(f.run().copied == 0);
    assert!(within_tolerance(
        mtime(&f.source.join("sub")),
        mtime(&f.dest.join("sub"))
    ));
}

#[test]
fn a_folder_changed_by_hand_is_put_right_at_a_full_check() {
    let f = Fixture::new();
    f.run();
    set_mtime(&f.dest.join("sub"), 100_000);
    assert!(!f.scan(false).has_work());
    let plan = f.scan(true);
    assert_eq!(plan.metadata.len(), 1);
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert!(within_tolerance(
        mtime(&f.source.join("sub")),
        mtime(&f.dest.join("sub"))
    ));
}

#[test]
fn an_attribute_change_alone_copies_nothing() {
    let f = Fixture::new();
    f.run();
    let source = f.source.join("sub").join("file.txt");
    let target = f.dest.join("sub").join("file.txt");
    let stamp = mtime(&target);
    let mut perms = fs::metadata(&source).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&source, perms).unwrap();

    let plan = f.scan(false);
    assert!(plan.copies.is_empty());
    assert_eq!(plan.metadata.len(), 1);
    let report = f.run();
    assert_eq!(report.copied, 0);
    assert_eq!(report.metadata, 1);
    assert!(fs::metadata(&target).unwrap().permissions().readonly());
    assert_eq!(mtime(&target), stamp);
}

#[test]
fn a_readonly_folder_stays_usable_in_the_destination() {
    let f = Fixture::new();
    let folder = f.source.join("sub");
    fs::write(folder.join("second.txt"), "second").unwrap();
    let mut perms = fs::metadata(&folder).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&folder, perms.clone()).unwrap();
    f.run();

    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&folder, perms).unwrap();
    fs::write(folder.join("third.txt"), "third").unwrap();
    f.run();
    assert!(f.dest.join("sub").join("third.txt").exists());
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::fs::{FileTimesExt, MetadataExt};

    fn set_created(path: &Path, seconds_ago: u64) {
        let time = SystemTime::now() - Duration::from_secs(seconds_ago);
        let file = open_for_times(path);
        file.set_times(fs::FileTimes::new().set_created(time))
            .unwrap();
    }

    fn created(path: &Path) -> u64 {
        fs::metadata(path).unwrap().creation_time()
    }

    #[test]
    fn a_copy_keeps_the_creation_date() {
        let f = Fixture::new();
        let source = f.source.join("sub").join("file.txt");
        set_created(&source, 100_000);
        f.run();
        assert_eq!(
            created(&source),
            created(&f.dest.join("sub").join("file.txt"))
        );
    }

    #[test]
    fn a_folder_keeps_its_creation_date() {
        let f = Fixture::new();
        set_created(&f.source.join("sub"), 100_000);
        f.run();
        assert_eq!(created(&f.source.join("sub")), created(&f.dest.join("sub")));
    }

    #[test]
    fn a_creation_date_change_alone_copies_nothing() {
        let f = Fixture::new();
        f.run();
        let source = f.source.join("sub").join("file.txt");
        set_created(&source, 100_000);
        let report = f.run();
        assert_eq!(report.copied, 0);
        assert_eq!(
            created(&source),
            created(&f.dest.join("sub").join("file.txt"))
        );
    }

    #[test]
    fn the_hidden_attribute_is_applied() {
        const HIDDEN: u32 = 0x2;
        let f = Fixture::new();
        f.run();
        let source = f.source.join("sub").join("file.txt");
        let status = std::process::Command::new("attrib")
            .arg("+H")
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());
        let report = f.run();
        assert_eq!(report.copied, 0);
        let attributes = fs::metadata(f.dest.join("sub").join("file.txt"))
            .unwrap()
            .file_attributes();
        assert_ne!(attributes & HIDDEN, 0);
    }
}

#[cfg(unix)]
#[test]
fn permission_bits_are_applied_without_copying() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.run();
    let source = f.source.join("sub").join("file.txt");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    let report = f.run();
    assert_eq!(report.copied, 0);
    let mode = fs::metadata(f.dest.join("sub").join("file.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o640);
}
