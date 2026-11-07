//! The overwrite safety check and flushed copies, on temporary folders only.

use hayakagami::exec;
use hayakagami::model::{MirrorError, PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, Plan};
use std::fs;
use std::path::PathBuf;
use std::sync::Once;
use tempfile::TempDir;

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    dest: PathBuf,
}

impl Fixture {
    /// A source of `files` files, already mirrored once.
    fn seeded(files: usize) -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hkgm-safety-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        for index in 0..files {
            fs::write(source.join(format!("f{index:02}.txt")), "original").unwrap();
        }
        let f = Self {
            _root: root,
            source,
            dest,
        };
        exec::execute(f.scan(), &RunContext::silent(), false).unwrap();
        f
    }

    fn scan(&self) -> Plan {
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
        };
        plan::scan(&spec, &ScanOptions::default(), &RunContext::silent()).unwrap()
    }

    fn damage(&self, count: usize) {
        for index in 0..count {
            let name = format!("f{index:02}.txt");
            fs::write(self.source.join(name), "encrypted, and longer").unwrap();
        }
    }

    fn untouched(&self, index: usize) -> bool {
        let name = format!("f{index:02}.txt");
        fs::read_to_string(self.dest.join(name)).unwrap() == "original"
    }
}

#[test]
fn overwriting_a_tenth_stops_and_changes_nothing() {
    let f = Fixture::seeded(20);
    f.damage(2);
    let plan = f.scan();
    assert!(plan.guard.is_some());
    let result = exec::execute(plan, &RunContext::silent(), false);
    assert!(matches!(result, Err(MirrorError::Guard(_))));
    assert!(f.untouched(0));
}

#[test]
fn overwriting_less_than_a_tenth_goes_ahead() {
    let f = Fixture::seeded(20);
    f.damage(1);
    let plan = f.scan();
    assert!(plan.guard.is_none());
    exec::execute(plan, &RunContext::silent(), false).unwrap();
    assert!(!f.untouched(0));
}

#[test]
fn an_approved_overwrite_is_carried_out() {
    let f = Fixture::seeded(20);
    f.damage(20);
    exec::execute(f.scan(), &RunContext::silent(), true).unwrap();
    assert!(!f.untouched(0));
    assert!(!f.scan().has_work());
}

#[test]
fn a_small_destination_is_not_stopped() {
    let f = Fixture::seeded(9);
    f.damage(9);
    assert!(f.scan().guard.is_none());
}

/// New files are not a sign of damage, however many there are.
#[test]
fn adding_files_is_not_stopped() {
    let f = Fixture::seeded(10);
    for index in 0..50 {
        fs::write(f.source.join(format!("new{index}.txt")), "new").unwrap();
    }
    assert!(f.scan().guard.is_none());
}

/// Flushing opens the finished copy for writing, which a read-only file refuses
/// unless it is handled.
#[test]
fn a_readonly_file_is_copied_and_stays_readonly() {
    let f = Fixture::seeded(1);
    let source = f.source.join("readonly.txt");
    fs::write(&source, "readonly").unwrap();
    let mut perms = fs::metadata(&source).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&source, perms).unwrap();
    let report = exec::execute(f.scan(), &RunContext::silent(), true).unwrap();
    assert_eq!(report.errors, 0);
    let copied = f.dest.join("readonly.txt");
    assert_eq!(fs::read_to_string(&copied).unwrap(), "readonly");
    assert!(fs::metadata(&copied).unwrap().permissions().readonly());
}
