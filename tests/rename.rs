//! Moving over from the earlier name of this program, on temporary folders only.

use hikae::config;
use hikae::exec;
use hikae::model::{PairSpec, RunContext, ScanOptions};
use hikae::plan::{self, DestScan, Plan};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tempfile::TempDir;

const OLD_MARKER: &str = ".hayakagami-state";
const NEW_MARKER: &str = ".hikae-state";

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    dest: PathBuf,
}

impl Fixture {
    /// One file, mirrored once, as the earlier version would have left it.
    fn mirrored_by_old_version() -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hikae-rename-{}", std::process::id()));
            std::env::set_var("HIKAE_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.txt"), "one").unwrap();
        let f = Self {
            _root: root,
            source,
            dest,
        };
        exec::execute(f.scan(false), &RunContext::silent(), false).unwrap();
        fs::rename(f.dest.join(NEW_MARKER), f.dest.join(OLD_MARKER)).unwrap();
        f
    }

    fn scan(&self, force_full: bool) -> Plan {
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
            expected_disk: None,
        };
        let options = ScanOptions {
            force_full,
            ..ScanOptions::default()
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }
}

#[test]
fn an_old_marker_keeps_the_saved_state_usable() {
    let f = Fixture::mirrored_by_old_version();
    let plan = f.scan(false);
    assert!(!matches!(plan.dest_scan, DestScan::Full(_)));
    assert!(plan.guard.is_none());
    assert!(!plan.has_work());
}

#[test]
fn a_destination_with_an_old_marker_counts_as_mirrored_before() {
    let f = Fixture::mirrored_by_old_version();
    fs::remove_file(f.source.join("a.txt")).unwrap();
    fs::write(f.source.join("b.txt"), "two").unwrap();
    assert!(f.scan(true).guard.is_none());
}

#[test]
fn a_full_check_keeps_the_old_marker_out_of_the_plan() {
    let f = Fixture::mirrored_by_old_version();
    assert!(!f.scan(true).has_work());
    assert!(f.dest.join(OLD_MARKER).exists());
}

#[test]
fn a_run_replaces_the_old_marker() {
    let f = Fixture::mirrored_by_old_version();
    fs::write(f.source.join("b.txt"), "two").unwrap();
    exec::execute(f.scan(false), &RunContext::silent(), false).unwrap();
    assert!(f.dest.join("b.txt").exists());
    assert!(f.dest.join(NEW_MARKER).exists());
    assert!(!f.dest.join(OLD_MARKER).exists());
    assert!(!matches!(f.scan(false).dest_scan, DestScan::Full(_)));
}

#[test]
fn an_old_marker_in_the_source_is_not_copied() {
    let f = Fixture::mirrored_by_old_version();
    fs::write(f.source.join(OLD_MARKER), "from another backup").unwrap();
    fs::write(f.source.join("b.txt"), "two").unwrap();
    exec::execute(f.scan(true), &RunContext::silent(), false).unwrap();
    assert!(!f.dest.join(OLD_MARKER).exists());
}

#[test]
fn an_old_temporary_copy_is_removed_by_a_full_check() {
    let f = Fixture::mirrored_by_old_version();
    let leftover = f.dest.join(".hkgm1-0.hkgmtmp");
    fs::write(&leftover, "partial").unwrap();
    exec::execute(f.scan(true), &RunContext::silent(), false).unwrap();
    assert!(!leftover.exists());
}

#[test]
fn an_old_pattern_file_still_opens() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("old.hkgm");
    fs::write(
        &path,
        r#"{"format":"hayakagami-pattern","version":1,"name":"old"}"#,
    )
    .unwrap();
    assert!(config::is_pattern_file(&path));
    assert!(config::is_pattern_file(Path::new("new.hikae")));
    assert!(!config::is_pattern_file(Path::new("saved-name")));
    assert_eq!(config::load_pattern_file(&path).unwrap().name, "old");
}

struct Folders {
    _root: TempDir,
    old_config: PathBuf,
    new_config: PathBuf,
    old_cache: PathBuf,
    new_cache: PathBuf,
}

impl Folders {
    fn with_old_settings() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = |name: &str| root.path().join(name);
        let f = Self {
            old_config: path("config/Hayakagami"),
            new_config: path("config/Hikae"),
            old_cache: path("cache/Hayakagami"),
            new_cache: path("cache/Hikae"),
            _root: root,
        };
        fs::create_dir_all(&f.old_config).unwrap();
        fs::create_dir_all(&f.old_cache).unwrap();
        fs::write(f.old_config.join("patterns.json"), "old patterns").unwrap();
        fs::write(f.old_cache.join("pair.bin"), "old state").unwrap();
        f
    }

    fn migrate(&self) -> Vec<String> {
        config::migrate_folders(
            &self.old_config,
            &self.new_config,
            &self.old_cache,
            &self.new_cache,
        )
    }
}

#[test]
fn patterns_are_copied_and_the_originals_stay() {
    let f = Folders::with_old_settings();
    assert!(f.migrate().is_empty());
    let new = f.new_config.join("patterns.json");
    assert_eq!(fs::read_to_string(new).unwrap(), "old patterns");
    let old = f.old_config.join("patterns.json");
    assert_eq!(fs::read_to_string(old).unwrap(), "old patterns");
}

#[test]
fn saved_state_is_moved() {
    let f = Folders::with_old_settings();
    assert!(f.migrate().is_empty());
    let moved = f.new_cache.join("pair.bin");
    assert_eq!(fs::read_to_string(moved).unwrap(), "old state");
    assert!(!f.old_cache.exists());
}

#[test]
fn settings_already_in_the_new_place_are_kept() {
    let f = Folders::with_old_settings();
    fs::create_dir_all(&f.new_config).unwrap();
    fs::create_dir_all(&f.new_cache).unwrap();
    fs::write(f.new_config.join("patterns.json"), "new patterns").unwrap();
    assert!(f.migrate().is_empty());
    let kept = f.new_config.join("patterns.json");
    assert_eq!(fs::read_to_string(kept).unwrap(), "new patterns");
    assert!(f.old_cache.join("pair.bin").exists());
}

#[test]
fn nothing_to_bring_over_is_not_a_problem() {
    let f = Folders::with_old_settings();
    fs::remove_dir_all(&f.old_config).unwrap();
    fs::remove_dir_all(&f.old_cache).unwrap();
    assert!(f.migrate().is_empty());
    assert!(!f.new_config.exists());
    assert!(!f.new_cache.exists());
}
