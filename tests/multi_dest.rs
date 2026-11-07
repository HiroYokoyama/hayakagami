//! One source mirrored to several destinations, on temporary folders only.
//!
//! Each destination must keep its own state: a run to one of them must not make
//! another miss a change.

use hayakagami::exec;
use hayakagami::model::{PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan, SourceScan};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tempfile::TempDir;

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hkgm-multi-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("top.txt"), "top").unwrap();
        fs::write(source.join("sub").join("one.txt"), "one").unwrap();
        Self {
            a: root.path().join("a"),
            b: root.path().join("b"),
            source,
            _root: root,
        }
    }

    fn scan(&self, dest: &Path, use_journal: bool) -> Plan {
        let options = ScanOptions {
            use_journal,
            ..ScanOptions::default()
        };
        let spec = PairSpec {
            source: self.source.clone(),
            dest: dest.to_path_buf(),
            excludes: Vec::new(),
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }

    fn run(&self, dest: &Path, use_journal: bool) {
        exec::execute(self.scan(dest, use_journal), &RunContext::silent(), false).unwrap();
    }

    fn seed(&self, use_journal: bool) {
        self.run(&self.a, use_journal);
        self.run(&self.b, use_journal);
    }
}

#[test]
fn both_destinations_get_a_full_copy_and_then_rest() {
    let f = Fixture::new();
    f.seed(false);
    for dest in [&f.a, &f.b] {
        assert_eq!(fs::read_to_string(dest.join("top.txt")).unwrap(), "top");
        let plan = f.scan(dest, false);
        assert_eq!(plan.dest_scan, DestScan::Cached);
        assert!(!plan.has_work());
    }
}

#[test]
fn a_run_to_one_destination_leaves_the_change_for_the_other() {
    let f = Fixture::new();
    f.seed(false);
    fs::write(f.source.join("sub").join("new.txt"), "new").unwrap();
    f.run(&f.a, false);
    assert!(f.a.join("sub").join("new.txt").exists());
    assert!(!f.b.join("sub").join("new.txt").exists());
    assert_eq!(f.scan(&f.b, false).copies.len(), 1);
    f.run(&f.b, false);
    assert!(f.b.join("sub").join("new.txt").exists());
}

#[test]
fn a_deletion_reaches_every_destination() {
    let f = Fixture::new();
    f.seed(false);
    fs::remove_file(f.source.join("top.txt")).unwrap();
    f.seed(false);
    assert!(!f.a.join("top.txt").exists());
    assert!(!f.b.join("top.txt").exists());
}

#[test]
fn trouble_in_one_destination_does_not_affect_the_other() {
    let f = Fixture::new();
    f.seed(false);
    fs::remove_file(f.a.join(".hayakagami-state")).unwrap();
    assert!(matches!(f.scan(&f.a, false).dest_scan, DestScan::Full(_)));
    assert_eq!(f.scan(&f.b, false).dest_scan, DestScan::Cached);
}

/// Sharing one journal position would make this test fail: the run to `a` would
/// move it past the change that `b` has not received.
#[test]
fn each_destination_keeps_its_own_journal_position() {
    let f = Fixture::new();
    f.seed(true);
    // The first run with saved listings shows whether the journal is readable here.
    let probe = f.scan(&f.a, true);
    if !matches!(probe.source_scan, SourceScan::Journal { .. }) {
        assert!(
            std::env::var_os("HAYAKAGAMI_REQUIRE_USN").is_none() || !cfg!(windows),
            "the journal was not used: {:?}",
            probe.source_scan
        );
        eprintln!("skipped: {:?}", probe.source_scan);
        return;
    }
    fs::write(f.source.join("sub").join("new.txt"), "new").unwrap();
    f.run(&f.a, true);
    assert!(f.a.join("sub").join("new.txt").exists());

    let plan = f.scan(&f.b, true);
    assert!(matches!(plan.source_scan, SourceScan::Journal { .. }));
    assert_eq!(plan.copies.len(), 1);
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(f.b.join("sub").join("new.txt").exists());
}
