//! The change journal shortcut, on temporary folders only.
//!
//! Reading a real journal needs administrator rights on most drives. Tests that
//! need it are skipped without them, except where HAYAKAGAMI_REQUIRE_USN is set,
//! as it is on the Windows CI runner.
#![cfg(windows)]

use hayakagami::exec;
use hayakagami::journal::{Cursor, Keys, ReadFailure, RemoteHandle, RemoteJournal};
use hayakagami::model::{PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, Plan, SourceScan};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
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
            let home = std::env::temp_dir().join(format!("hkgm-usn-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        for folder in ["a/b", "c", "d"] {
            fs::create_dir_all(source.join(folder)).unwrap();
        }
        for file in [
            "top.txt",
            "a/one.txt",
            "a/b/two.txt",
            "c/three.txt",
            "d/four.txt",
        ] {
            fs::write(source.join(file), file).unwrap();
        }
        Self {
            _root: root,
            source,
            dest,
        }
    }

    fn scan(&self, remote: Option<RemoteHandle>) -> Plan {
        let options = ScanOptions {
            use_journal: true,
            journal_remote: remote,
            ..ScanOptions::default()
        };
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }

    fn run(&self, plan: Plan) {
        exec::execute(plan, &RunContext::silent(), true).unwrap();
    }

    /// Copies once, so the next scan has saved listings to use.
    fn seed(&self) {
        let plan = self.scan(None);
        assert!(matches!(plan.source_scan, SourceScan::JournalUnused { .. }));
        self.run(plan);
    }
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            if rel == ".hayakagami-state" {
                continue;
            }
            if path.is_dir() {
                out.push((rel, Vec::new()));
                walk(root, &path, out);
            } else {
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// True when this process can read the journal itself.
fn readable(plan: &Plan) -> bool {
    if matches!(plan.source_scan, SourceScan::Journal { .. }) {
        return true;
    }
    assert!(
        std::env::var_os("HAYAKAGAMI_REQUIRE_USN").is_none(),
        "the journal was not used: {:?}",
        plan.source_scan
    );
    eprintln!("skipped: {:?}", plan.source_scan);
    false
}

/// Scans through the journal, applies the plan and checks the result against the
/// source itself, so a change the journal missed fails the test.
fn check_change(change: impl FnOnce(&Fixture)) {
    let f = Fixture::new();
    f.seed();
    let unchanged = f.scan(None);
    if !readable(&unchanged) {
        return;
    }
    assert!(!unchanged.has_work());
    f.run(unchanged);
    change(&f);
    let plan = f.scan(None);
    assert!(readable(&plan));
    f.run(plan);
    assert_eq!(tree(&f.source), tree(&f.dest));
}

/// A change can be found after many unchanged runs that added only a position.
#[test]
fn unchanged_runs_add_little_and_lose_nothing() {
    let f = Fixture::new();
    f.seed();
    let index = hayakagami::journal::index_path(f.scan(None).state_file());
    let first = f.scan(None);
    if !readable(&first) {
        return;
    }
    let full = fs::metadata(&index).unwrap().len();
    f.run(first);
    for _ in 0..5 {
        f.run(f.scan(None));
    }
    let added = fs::metadata(&index).unwrap().len() - full;
    // Other programs can touch the temporary folder's parent, which is outside
    // the source, so these six runs add only a position each.
    assert!(added < 1000, "added {added} bytes");

    fs::write(f.source.join("c/three.txt"), "edited, and longer").unwrap();
    let plan = f.scan(None);
    assert!(readable(&plan));
    f.run(plan);
    assert_eq!(tree(&f.source), tree(&f.dest));
}

/// Losing the last position must mean reading the journal again, not missing a change.
#[test]
fn a_torn_position_reads_the_journal_again() {
    let f = Fixture::new();
    f.seed();
    let index = hayakagami::journal::index_path(f.scan(None).state_file());
    if !readable(&f.scan(None)) {
        return;
    }
    f.run(f.scan(None));
    fs::write(f.source.join("a/one.txt"), "edited, and longer").unwrap();
    f.run(f.scan(None));
    let bytes = fs::read(&index).unwrap();
    fs::write(&index, &bytes[..bytes.len() - 1]).unwrap();

    fs::write(f.source.join("d/four.txt"), "also edited, and longer").unwrap();
    let plan = f.scan(None);
    assert!(readable(&plan));
    f.run(plan);
    assert_eq!(tree(&f.source), tree(&f.dest));
}

#[test]
fn an_edited_file_is_found() {
    check_change(|f| fs::write(f.source.join("a/b/two.txt"), "edited, longer").unwrap());
}

#[test]
fn a_new_file_is_found() {
    check_change(|f| fs::write(f.source.join("c/new.txt"), "new").unwrap());
}

#[test]
fn a_deleted_file_is_found() {
    check_change(|f| fs::remove_file(f.source.join("c/three.txt")).unwrap());
}

#[test]
fn a_new_folder_is_found() {
    check_change(|f| {
        fs::create_dir_all(f.source.join("d/x/y")).unwrap();
        fs::write(f.source.join("d/x/y/deep.txt"), "deep").unwrap();
    });
}

#[test]
fn a_deleted_folder_is_found() {
    check_change(|f| fs::remove_dir_all(f.source.join("a")).unwrap());
}

#[test]
fn a_renamed_folder_is_found() {
    check_change(|f| fs::rename(f.source.join("a"), f.source.join("renamed")).unwrap());
}

#[test]
fn a_moved_folder_is_found() {
    check_change(|f| fs::rename(f.source.join("a/b"), f.source.join("c/b")).unwrap());
}

/// The old tree's listings must not be used for the new folder of the same name.
#[test]
fn a_folder_replaced_under_the_same_name_is_found() {
    check_change(|f| {
        fs::rename(f.source.join("a"), f.source.join("old")).unwrap();
        fs::create_dir_all(f.source.join("a/b")).unwrap();
        fs::write(f.source.join("a/b/other.txt"), "other").unwrap();
    });
}

#[test]
fn a_folder_changed_and_then_deleted_is_found() {
    check_change(|f| {
        fs::write(f.source.join("d/temp.txt"), "temp").unwrap();
        fs::remove_dir_all(f.source.join("d")).unwrap();
    });
}

/// Answers for a process without administrator rights, and records the question.
struct Fake {
    keys: Keys,
    calls: Mutex<u32>,
}

impl RemoteJournal for Fake {
    fn read(&self, _source: &Path, _from: Cursor, _until: i64) -> Result<Keys, ReadFailure> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.keys.clone())
    }
}

fn fake(listed: &[&str]) -> (Arc<Fake>, RemoteHandle) {
    let fake = Arc::new(Fake {
        keys: Keys {
            listed: listed
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<HashSet<_>>(),
            trees: HashSet::new(),
        },
        calls: Mutex::new(0),
    });
    (fake.clone(), RemoteHandle(fake))
}

/// Only reachable without rights, because otherwise the journal is read directly.
fn denied_here(f: &Fixture) -> bool {
    let plan = f.scan(None);
    matches!(
        plan.source_scan,
        SourceScan::JournalUnused { denied: true, .. }
    )
}

#[test]
fn only_folders_named_by_the_journal_are_listed() {
    let f = Fixture::new();
    f.seed();
    if !denied_here(&f) {
        eprintln!("skipped: this process can read the journal");
        return;
    }
    // Two folders change on disk, but the helper names only one. Seeing just
    // that one proves the other's listing came from the saved copy.
    fs::write(f.source.join("c/seen.txt"), "seen").unwrap();
    fs::write(f.source.join("d/unseen.txt"), "unseen").unwrap();
    let (answers, remote) = fake(&["c"]);
    let plan = f.scan(Some(remote));
    assert_eq!(*answers.calls.lock().unwrap(), 1);
    assert_eq!(plan.source_scan, SourceScan::Journal { changed_folders: 1 });
    let copies: Vec<&PathBuf> = plan.copies.iter().map(|c| &c.rel).collect();
    assert_eq!(copies, [&Path::new("c").join("seen.txt")]);
}

#[test]
fn turning_the_journal_off_lists_everything() {
    let f = Fixture::new();
    f.seed();
    fs::write(f.source.join("d/after.txt"), "after").unwrap();
    let spec = PairSpec {
        source: f.source.clone(),
        dest: f.dest.clone(),
        excludes: Vec::new(),
    };
    let plan = plan::scan(&spec, &ScanOptions::default(), &RunContext::silent()).unwrap();
    assert_eq!(plan.source_scan, SourceScan::Disk);
    assert_eq!(plan.copies.len(), 1);
}
