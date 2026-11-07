//! Copying links as links, on temporary folders only.

use hayakagami::exec;
use hayakagami::model::{PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, Plan};
use hayakagami::platform;
#[cfg(windows)]
use hayakagami::platform::LinkKind;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tempfile::TempDir;

static ISOLATE: Once = Once::new();

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    dest: PathBuf,
    /// Outside both trees, so anything written through a link shows up here.
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        ISOLATE.call_once(|| {
            let home = std::env::temp_dir().join(format!("hkgm-links-{}", std::process::id()));
            std::env::set_var("HAYAKAGAMI_HOME", home);
        });
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        let outside = root.path().join("outside");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("kept.txt"), "outside").unwrap();
        fs::write(source.join("plain.txt"), "plain").unwrap();
        Self {
            _root: root,
            source,
            dest,
            outside,
        }
    }

    fn scan(&self, copy_links: bool) -> Plan {
        let options = ScanOptions {
            copy_links,
            ..ScanOptions::default()
        };
        let spec = PairSpec {
            source: self.source.clone(),
            dest: self.dest.clone(),
            excludes: Vec::new(),
        };
        plan::scan(&spec, &options, &RunContext::silent()).unwrap()
    }

    fn run(&self) -> Plan {
        let plan = self.scan(true);
        let links = plan.links.clone();
        let copies = plan.copies.clone();
        exec::execute(plan, &RunContext::silent(), true).unwrap();
        let again = self.scan(true);
        assert!(
            !again.has_work(),
            "{links:?} {copies:?} then {:?}",
            again.links
        );
        again
    }

    fn outside_is_untouched(&self) {
        assert_eq!(
            fs::read_to_string(self.outside.join("kept.txt")).unwrap(),
            "outside"
        );
    }
}

#[cfg(unix)]
fn file_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

/// False when this account may not make symbolic links, as on a normal Windows
/// account without Developer Mode.
#[cfg(windows)]
fn file_link(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(e) if platform::link_unavailable(&e) => {
            assert!(
                std::env::var_os("HAYAKAGAMI_REQUIRE_USN").is_none(),
                "symbolic links are required on CI: {e}"
            );
            eprintln!("skipped: {e}");
            false
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn an_absolute_link_keeps_its_target() {
    let f = Fixture::new();
    let target = f.outside.join("kept.txt");
    if !file_link(&target, &f.source.join("link.txt")) {
        return;
    }
    f.run();
    let copied = platform::read_link(&f.dest.join("link.txt")).unwrap();
    assert_eq!(copied.target, target);
    f.outside_is_untouched();
}

#[test]
fn a_relative_link_stays_relative() {
    let f = Fixture::new();
    if !file_link(Path::new("plain.txt"), &f.source.join("link.txt")) {
        return;
    }
    f.run();
    let copied = platform::read_link(&f.dest.join("link.txt")).unwrap();
    assert_eq!(copied.target, Path::new("plain.txt"));
}

#[test]
fn a_changed_target_is_applied() {
    let f = Fixture::new();
    let link = f.source.join("link.txt");
    if !file_link(&f.outside.join("kept.txt"), &link) {
        return;
    }
    f.run();
    fs::remove_file(&link).unwrap();
    assert!(file_link(&f.outside.join("other.txt"), &link));
    f.run();
    let copied = platform::read_link(&f.dest.join("link.txt")).unwrap();
    assert_eq!(copied.target, f.outside.join("other.txt"));
    f.outside_is_untouched();
}

#[test]
fn a_file_in_the_destination_is_replaced_by_a_link() {
    let f = Fixture::new();
    fs::write(f.source.join("swap.txt"), "file").unwrap();
    f.run();
    fs::remove_file(f.source.join("swap.txt")).unwrap();
    if !file_link(&f.outside.join("kept.txt"), &f.source.join("swap.txt")) {
        return;
    }
    f.run();
    assert!(platform::read_link(&f.dest.join("swap.txt")).is_ok());
    f.outside_is_untouched();
}

/// Replacing the link must not write into what it points to.
#[test]
fn a_link_in_the_destination_is_not_written_through() {
    let f = Fixture::new();
    if !file_link(&f.outside.join("kept.txt"), &f.source.join("swap.txt")) {
        return;
    }
    f.run();
    fs::remove_file(f.source.join("swap.txt")).unwrap();
    fs::write(f.source.join("swap.txt"), "now a file").unwrap();
    f.run();
    assert!(platform::read_link(&f.dest.join("swap.txt")).is_err());
    assert_eq!(
        fs::read_to_string(f.dest.join("swap.txt")).unwrap(),
        "now a file"
    );
    f.outside_is_untouched();
}

#[test]
fn a_removed_link_is_deleted_and_its_target_stays() {
    let f = Fixture::new();
    if !file_link(&f.outside.join("kept.txt"), &f.source.join("link.txt")) {
        return;
    }
    f.run();
    fs::remove_file(f.source.join("link.txt")).unwrap();
    f.run();
    assert!(fs::symlink_metadata(f.dest.join("link.txt")).is_err());
    f.outside_is_untouched();
}

#[test]
fn links_stay_uncopied_when_the_option_is_off() {
    let f = Fixture::new();
    if !file_link(&f.outside.join("kept.txt"), &f.source.join("link.txt")) {
        return;
    }
    let plan = f.scan(false);
    assert!(plan.links.is_empty());
    assert_eq!(plan.warnings.len(), 1);
    exec::execute(plan, &RunContext::silent(), true).unwrap();
    assert!(fs::symlink_metadata(f.dest.join("link.txt")).is_err());
}

#[cfg(windows)]
fn junction(target: &Path, link: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn a_junction_is_recreated_with_its_target() {
    let f = Fixture::new();
    junction(&f.outside, &f.source.join("jct"));
    let original = platform::read_link(&f.source.join("jct")).unwrap();
    assert_eq!(original.kind, LinkKind::Junction);
    f.run();
    let copied = platform::read_link(&f.dest.join("jct")).unwrap();
    assert_eq!(copied, original);
    // It really works as a junction, reaching the same folder.
    assert_eq!(
        fs::read_to_string(f.dest.join("jct").join("kept.txt")).unwrap(),
        "outside"
    );
}

#[cfg(windows)]
#[test]
fn a_real_folder_in_the_destination_is_replaced_by_a_junction() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("swap")).unwrap();
    fs::write(f.source.join("swap").join("old.txt"), "old").unwrap();
    f.run();
    fs::remove_dir_all(f.source.join("swap")).unwrap();
    junction(&f.outside, &f.source.join("swap"));
    f.run();
    assert_eq!(
        platform::read_link(&f.dest.join("swap")).unwrap().kind,
        LinkKind::Junction
    );
    f.outside_is_untouched();
}

/// Removing a copied junction removes the link only.
#[cfg(windows)]
#[test]
fn a_removed_junction_leaves_its_target() {
    let f = Fixture::new();
    junction(&f.outside, &f.source.join("jct"));
    f.run();
    fs::remove_dir(f.source.join("jct")).unwrap();
    f.run();
    assert!(fs::symlink_metadata(f.dest.join("jct")).is_err());
    f.outside_is_untouched();
}
