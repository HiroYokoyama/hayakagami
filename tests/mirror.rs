//! End-to-end behaviour on temporary folders only.

use hikae::exec;
use hikae::model::{MirrorError, PairSpec, RunContext, ScanOptions};
use hikae::plan::{self, DestScan, Plan};
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
    excludes: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hikae-test-{}", std::process::id()));
            std::env::set_var("HIKAE_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        Self {
            _root: root,
            source,
            dest,
            excludes: Vec::new(),
        }
    }

    fn spec(&self) -> PairSpec {
        PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: self.excludes.clone(),
            expected_disk: None,
        }
    }

    fn scan(&self) -> Plan {
        plan::scan(&self.spec(), &ScanOptions::default(), &RunContext::silent()).unwrap()
    }

    fn scan_full(&self) -> Plan {
        let options = ScanOptions {
            force_full: true,
            ..ScanOptions::default()
        };
        plan::scan(&self.spec(), &options, &RunContext::silent()).unwrap()
    }

    fn run(&self) -> Plan {
        let plan = self.scan();
        let summary = self.scan();
        exec::execute(plan, &RunContext::silent(), false).unwrap();
        summary
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn set_old_mtime(path: &Path, seconds: u64) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(seconds))
        .unwrap();
}

#[test]
fn copies_updates_and_removes() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    write(&f.source.join("sub/日本語.txt"), "two");
    f.run();
    assert_eq!(fs::read_to_string(f.dest.join("a.txt")).unwrap(), "one");
    assert_eq!(
        fs::read_to_string(f.dest.join("sub/日本語.txt")).unwrap(),
        "two"
    );

    write(&f.source.join("a.txt"), "changed");
    fs::remove_file(f.source.join("sub/日本語.txt")).unwrap();
    fs::remove_dir(f.source.join("sub")).unwrap();
    f.run();
    assert_eq!(fs::read_to_string(f.dest.join("a.txt")).unwrap(), "changed");
    assert!(!f.dest.join("sub").exists());
}

#[test]
fn modification_time_is_preserved_and_second_run_is_idle() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    set_old_mtime(&f.source.join("a.txt"), 5000);
    f.run();
    let source_time = fs::metadata(f.source.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let dest_time = fs::metadata(f.dest.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let gap = source_time
        .duration_since(dest_time)
        .or_else(|e| Ok::<_, ()>(e.duration()))
        .unwrap();
    assert!(gap < Duration::from_secs(2));
    assert!(!f.scan().has_work());
    assert!(!f.scan_full().has_work());
}

#[test]
fn excluded_items_are_neither_copied_nor_deleted() {
    let mut f = Fixture::new();
    f.excludes = vec!["*.log".into(), "cache/".into(), "docs/draft".into()];
    write(&f.source.join("keep.txt"), "k");
    write(&f.source.join("x.LOG"), "log");
    write(&f.source.join("cache/c.bin"), "c");
    write(&f.source.join("docs/draft"), "d");
    write(&f.source.join("docs/final"), "f");
    f.run();
    assert!(f.dest.join("keep.txt").exists());
    assert!(f.dest.join("docs/final").exists());
    assert!(!f.dest.join("x.LOG").exists());
    assert!(!f.dest.join("cache").exists());
    assert!(!f.dest.join("docs/draft").exists());

    write(&f.dest.join("only-dest.log"), "precious");
    let plan = f.scan_full();
    assert!(plan.delete_files.is_empty());
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(f.dest.join("only-dest.log").exists());
}

#[test]
fn file_and_folder_can_swap_places() {
    let f = Fixture::new();
    write(&f.source.join("a"), "file");
    write(&f.source.join("b/inner.txt"), "dir");
    write(&f.source.join("other.txt"), "stay");
    f.run();
    fs::remove_file(f.source.join("a")).unwrap();
    fs::remove_dir_all(f.source.join("b")).unwrap();
    write(&f.source.join("a/inner.txt"), "now dir");
    write(&f.source.join("b"), "now file");
    let plan = f.scan();
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert_eq!(
        fs::read_to_string(f.dest.join("a/inner.txt")).unwrap(),
        "now dir"
    );
    assert_eq!(fs::read_to_string(f.dest.join("b")).unwrap(), "now file");
}

#[test]
fn guard_empty_source_protects_destination() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    f.run();
    fs::remove_file(f.source.join("a.txt")).unwrap();
    let plan = f.scan();
    assert!(plan.guard.is_some());
    let result = exec::execute(plan, &RunContext::silent(), false);
    assert!(matches!(result, Err(MirrorError::Guard(_))));
    assert!(f.dest.join("a.txt").exists());
}

#[test]
fn guard_mass_delete_protects_destination() {
    let f = Fixture::new();
    for i in 0..20 {
        write(&f.source.join(format!("{i}.txt")), "x");
    }
    f.run();
    for i in 0..15 {
        fs::remove_file(f.source.join(format!("{i}.txt"))).unwrap();
    }
    let plan = f.scan();
    assert!(plan.guard.is_some());
    assert!(exec::execute(plan, &RunContext::silent(), false).is_err());
    assert!(f.dest.join("0.txt").exists());
}

#[test]
fn guard_unknown_destination_protects_existing_files() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    write(&f.dest.join("unrelated.txt"), "someone else's data");
    let plan = f.scan();
    assert!(plan.guard.is_some());
    assert!(exec::execute(plan, &RunContext::silent(), false).is_err());
    assert!(f.dest.join("unrelated.txt").exists());
    assert!(!f.dest.join("a.txt").exists());
}

#[test]
fn unknown_destination_without_deletions_is_allowed() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    fs::create_dir_all(&f.dest).unwrap();
    let plan = f.scan();
    assert!(plan.guard.is_none());
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(f.dest.join("a.txt").exists());
}

#[test]
fn overlapping_folders_are_refused() {
    let f = Fixture::new();
    for dest in [f.source.join("inner"), f.source.clone()] {
        let spec = PairSpec {
            source: f.source.clone(),
            dest,
            excludes: Vec::new(),
            expected_disk: None,
        };
        let result = plan::scan(&spec, &ScanOptions::default(), &RunContext::silent());
        assert!(matches!(result, Err(MirrorError::Guard(_))));
    }
    let spec = PairSpec {
        source: f.source.join("missing"),
        dest: f.dest.clone(),
        excludes: Vec::new(),
        expected_disk: None,
    };
    assert!(plan::scan(&spec, &ScanOptions::default(), &RunContext::silent()).is_err());
}

#[test]
fn scan_does_not_change_anything() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    let plan = f.scan();
    assert_eq!(plan.copies.len(), 1);
    assert!(!f.dest.exists());
}

#[test]
fn cache_is_used_and_detects_source_changes() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    write(&f.source.join("sub/b.txt"), "two");
    f.run();
    let plan = f.scan();
    assert_eq!(plan.dest_scan, DestScan::Cached);
    assert!(!plan.has_work());

    write(&f.source.join("sub/b.txt"), "changed content");
    write(&f.source.join("c.txt"), "new");
    let plan = f.scan();
    assert_eq!(plan.dest_scan, DestScan::Cached);
    assert_eq!(plan.copies.len(), 2);
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert_eq!(
        fs::read_to_string(f.dest.join("sub/b.txt")).unwrap(),
        "changed content"
    );
    let plan = f.scan();
    assert_eq!(plan.dest_scan, DestScan::Cached);
    assert!(!plan.has_work());
}

#[test]
fn cached_run_does_not_read_destination_changes_until_full_check() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    write(&f.source.join("b.txt"), "two");
    f.run();
    fs::remove_file(f.dest.join("a.txt")).unwrap();
    assert!(!f.scan().has_work());
    let plan = f.scan_full();
    assert_eq!(plan.copies.len(), 1);
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(f.dest.join("a.txt").exists());
}

#[test]
fn removed_marker_forces_full_scan() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    f.run();
    fs::remove_file(f.dest.join(".hikae-state")).unwrap();
    fs::remove_file(f.dest.join("a.txt")).unwrap();
    let plan = f.scan();
    assert!(matches!(plan.dest_scan, DestScan::Full(_)));
    assert_eq!(plan.copies.len(), 1);
}

#[test]
fn interrupted_run_forces_full_scan() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    f.run();
    fs::write(f.dest.join(".hikae-state"), "running-crashed").unwrap();
    assert!(matches!(f.scan().dest_scan, DestScan::Full(_)));
}

#[test]
fn leftover_temp_file_is_removed_by_full_scan() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    f.run();
    write(&f.dest.join(".hikae1-0.hikaetmp"), "partial");
    let plan = f.scan_full();
    assert_eq!(plan.delete_files.len(), 1);
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert!(!f.dest.join(".hikae1-0.hikaetmp").exists());
    assert!(f.dest.join("a.txt").exists());
}

#[test]
fn cancelled_scan_changes_nothing() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    let ctx = RunContext::silent();
    ctx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = plan::scan(&f.spec(), &ScanOptions::default(), &ctx);
    assert!(matches!(result, Err(MirrorError::Cancelled)));
    assert!(!f.dest.exists());
}

#[test]
fn readonly_destination_file_is_replaced() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "one");
    f.run();
    let mut perms = fs::metadata(f.dest.join("a.txt")).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(f.dest.join("a.txt"), perms).unwrap();
    write(&f.source.join("a.txt"), "second version");
    f.run();
    assert_eq!(
        fs::read_to_string(f.dest.join("a.txt")).unwrap(),
        "second version"
    );
}

#[cfg(unix)]
#[test]
fn link_in_destination_is_never_followed() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f.source.parent().unwrap().join("outside");
    write(&outside.join("precious.txt"), "keep me");
    write(&f.source.join("linked/a.txt"), "x");
    write(&f.source.join("other.txt"), "y");
    fs::create_dir_all(&f.dest).unwrap();
    symlink(&outside, f.dest.join("linked")).unwrap();
    symlink(&outside, f.dest.join("stray")).unwrap();
    let plan = f.scan();
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert_eq!(
        fs::read_to_string(outside.join("precious.txt")).unwrap(),
        "keep me"
    );
    assert!(!outside.join("a.txt").exists());
    assert!(fs::symlink_metadata(f.dest.join("linked"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(fs::symlink_metadata(f.dest.join("stray")).is_err());
}

#[cfg(unix)]
#[test]
fn unreadable_source_folder_keeps_its_destination_copy() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    write(&f.source.join("locked/a.txt"), "x");
    write(&f.source.join("b.txt"), "y");
    f.run();
    let locked = f.source.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        // Running as root: permissions cannot simulate the failure.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let plan = f.scan_full();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(plan.scan_errors, 1);
    assert!(plan.delete_files.is_empty());
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(f.dest.join("locked/a.txt").exists());
}

#[cfg(windows)]
#[test]
fn junction_in_destination_is_never_followed() {
    use std::process::Command;
    let f = Fixture::new();
    let outside = f.source.parent().unwrap().join("outside");
    write(&outside.join("precious.txt"), "keep me");
    write(&f.source.join("linked/a.txt"), "x");
    write(&f.source.join("other.txt"), "y");
    fs::create_dir_all(&f.dest).unwrap();
    for name in ["linked", "stray"] {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(f.dest.join(name))
            .arg(&outside)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    let plan = f.scan();
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert_eq!(
        fs::read_to_string(outside.join("precious.txt")).unwrap(),
        "keep me"
    );
    assert!(!outside.join("a.txt").exists());
    assert!(fs::symlink_metadata(f.dest.join("linked")).is_ok());
    assert!(fs::symlink_metadata(f.dest.join("stray")).is_err());
}

#[cfg(windows)]
#[test]
fn long_paths_work() {
    let f = Fixture::new();
    let mut deep = f.source.clone();
    for _ in 0..12 {
        deep.push("d".repeat(30));
    }
    let verbatim = PathBuf::from(format!(r"\\?\{}", deep.display()));
    fs::create_dir_all(&verbatim).unwrap();
    fs::write(verbatim.join("x.txt"), "deep").unwrap();
    let plan = f.scan();
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(!f.scan_full().has_work());
}

#[test]
fn guard_unknown_destination_protects_files_from_overwrite() {
    let f = Fixture::new();
    write(&f.source.join("a.txt"), "new content");
    write(&f.dest.join("a.txt"), "existing data");
    let plan = f.scan();
    assert!(plan.guard.is_some());
    assert!(exec::execute(plan, &RunContext::silent(), false).is_err());
    assert_eq!(
        fs::read_to_string(f.dest.join("a.txt")).unwrap(),
        "existing data"
    );
}
