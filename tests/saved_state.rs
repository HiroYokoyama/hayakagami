//! The saved destination state as an appended log, on temporary folders only.

use hayakagami::exec;
use hayakagami::model::{PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tempfile::TempDir;

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    dest: PathBuf,
}

impl Fixture {
    /// A mirrored source of `folders` folders holding 10 files each.
    fn seeded(folders: usize) -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hkgm-state-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        for folder in 0..folders {
            let dir = source.join(format!("folder{folder}"));
            fs::create_dir_all(&dir).unwrap();
            for file in 0..10 {
                fs::write(dir.join(format!("file{file}.txt")), "contents").unwrap();
            }
        }
        let f = Self {
            _root: root,
            source,
            dest,
        };
        f.run();
        f
    }

    fn scan(&self, force_full: bool) -> Plan {
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
        };
        let options = ScanOptions {
            force_full,
            ..ScanOptions::default()
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }

    fn run(&self) {
        exec::execute(self.scan(false), &RunContext::silent(), true).unwrap();
    }

    fn state_file(&self) -> PathBuf {
        self.scan(false).state_file().to_path_buf()
    }

    fn edit(&self, folder: usize, text: &str) {
        let path = self.source.join(format!("folder{folder}/file0.txt"));
        fs::write(path, text).unwrap();
    }

    fn assert_mirrored(&self) {
        assert_eq!(tree(&self.source), tree(&self.dest));
    }
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.file_name().unwrap() != ".hayakagami-state" {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

#[test]
fn a_run_with_nothing_to_do_writes_nothing() {
    let f = Fixture::seeded(5);
    let path = f.state_file();
    let before = fs::read(&path).unwrap();
    f.run();
    assert_eq!(before, fs::read(&path).unwrap());
}

#[test]
fn one_changed_file_adds_far_less_than_the_whole_state() {
    let f = Fixture::seeded(100);
    let path = f.state_file();
    let whole = len(&path);
    f.edit(3, "edited contents");
    f.run();
    let added = len(&path) - whole;
    assert!(added < whole / 20, "added {added} bytes to {whole}");
    f.assert_mirrored();
}

#[test]
fn the_appended_state_agrees_with_a_full_scan() {
    let f = Fixture::seeded(8);
    for round in 0..4 {
        f.edit(round, &format!("edit {round} of a longer length"));
        fs::write(
            f.source.join(format!("folder{round}/new{round}.txt")),
            "new",
        )
        .unwrap();
        fs::remove_file(f.source.join(format!("folder{}/file1.txt", round + 1))).unwrap();
        fs::create_dir(f.source.join(format!("made{round}"))).unwrap();
        fs::write(f.source.join(format!("made{round}/x.txt")), "x").unwrap();
        fs::remove_dir_all(f.source.join(format!("folder{}", 7 - round))).unwrap();
        f.run();
        f.assert_mirrored();

        let cached = f.scan(false);
        assert!(matches!(cached.dest_scan, DestScan::Cached));
        assert!(!cached.has_work());
        assert!(!f.scan(true).has_work());
    }
}

/// The state replayed from the log must describe the destination exactly, so a
/// copy made from it agrees file by file after a change on top of many appends.
#[test]
fn changes_after_many_appends_are_still_mirrored() {
    let f = Fixture::seeded(5);
    for round in 0..20 {
        // A different size each time: within 2 seconds, a file of the same size and
        // time counts as unchanged on macOS and Linux.
        f.edit(round % 5, &"edited".repeat(round + 2));
        f.run();
    }
    fs::remove_file(f.source.join("folder1/file3.txt")).unwrap();
    f.run();
    f.assert_mirrored();
    assert!(!f.scan(true).has_work());
}

#[test]
fn a_torn_tail_leads_to_a_full_scan_and_a_true_mirror() {
    let f = Fixture::seeded(5);
    let path = f.state_file();
    f.edit(1, "edited contents");
    f.run();
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();

    // Changed behind the state's back, which only reading the disk can notice.
    fs::remove_file(f.dest.join("folder2/file2.txt")).unwrap();
    let plan = f.scan(false);
    assert!(matches!(plan.dest_scan, DestScan::Full(_)));
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    f.assert_mirrored();

    // The log works again after the damage.
    f.edit(2, "edited again");
    f.run();
    f.assert_mirrored();
    assert!(matches!(f.scan(false).dest_scan, DestScan::Cached));
}

#[test]
fn an_unreadable_state_leads_to_a_full_scan() {
    let f = Fixture::seeded(3);
    fs::write(f.state_file(), b"HKGMCCH2 an older format").unwrap();
    assert!(matches!(f.scan(false).dest_scan, DestScan::Full(_)));
    f.run();
    assert!(matches!(f.scan(false).dest_scan, DestScan::Cached));
}
