//! Applies a plan to the destination.
//!
//! New contents are written to a temporary file and renamed into place, so an
//! interruption never leaves a half-written file under the real name. Deletion
//! runs after copying unless the disk lacks the space for that order.

use crate::cache::{self, CacheFile};
use crate::model::*;
use crate::plan::Plan;
use crate::platform;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Default, Clone, Debug)]
pub struct Report {
    pub copied: u64,
    pub copied_bytes: u64,
    pub deleted: u64,
    pub errors: u64,
    pub scan_errors: u64,
}

struct Shared<'a> {
    ctx: &'a RunContext,
    evict: Mutex<HashSet<PathBuf>>,
    disk_full: AtomicBool,
    counter: AtomicU64,
}

impl Shared<'_> {
    fn fail(&self, rel: &Path, action: &str, error: &io::Error) {
        self.ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
        self.ctx
            .log(format!("エラー({action}): {}: {error}", rel.display()));
        self.evict_parent(rel);
    }

    /// The next run reads this folder from disk instead of trusting the cache.
    fn evict_parent(&self, rel: &Path) {
        let mut evict = self.evict.lock().unwrap();
        evict.insert(rel.parent().unwrap_or(Path::new("")).to_path_buf());
        evict.insert(rel.to_path_buf());
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            // A failure surfaces in the retried operation.
            let _ = fs::set_permissions(path, perms);
        }
    }
}

fn retry_writable(path: &Path, op: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match op() {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            clear_readonly(path);
            op()
        }
        other => other,
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match retry_writable(path, || fs::remove_file(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_files(root: &Path, files: &[PathBuf], shared: &Shared) {
    for rel in files {
        if shared.ctx.cancelled() {
            return;
        }
        match remove_file(&root.join(rel)) {
            Ok(()) => {
                shared
                    .ctx
                    .progress
                    .deletes_done
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => shared.fail(rel, "削除", &e),
        }
    }
}

/// Removes folders with the non-recursive call only. A folder that still holds
/// something, such as an excluded file, stays in place.
fn remove_dirs(root: &Path, dirs: &[PathBuf], shared: &Shared) {
    for rel in dirs {
        if shared.ctx.cancelled() {
            return;
        }
        let path = root.join(rel);
        match retry_writable(&path, || fs::remove_dir(&path)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => shared.evict_parent(rel),
            Err(e) => shared.fail(rel, "フォルダ削除", &e),
        }
    }
}

fn copy_one(plan: &Plan, rel: &Path, shared: &Shared) -> io::Result<()> {
    let source = plan.source.join(rel);
    let target = plan.dest.join(rel);
    let id = shared.counter.fetch_add(1, Ordering::Relaxed);
    let tmp = target.with_file_name(format!(".hkgm{:x}-{id}{TMP_SUFFIX}", std::process::id()));
    let progress = &shared.ctx.progress;
    let copied = platform::copy_file(&source, &tmp, &shared.ctx.cancel, &mut |n| {
        progress.bytes_done.fetch_add(n, Ordering::Relaxed);
    });
    let result = copied.and_then(|()| retry_writable(&target, || fs::rename(&tmp, &target)));
    if result.is_err() {
        // The temporary file is ours and holds no unique data.
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn copy_all(plan: &Plan, shared: &Shared) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..plan.copy_threads.max(1) {
            scope.spawn(|| loop {
                if shared.ctx.cancelled() || shared.disk_full.load(Ordering::Relaxed) {
                    return;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = plan.copies.get(index) else {
                    return;
                };
                match copy_one(plan, &item.rel, shared) {
                    Ok(()) => {
                        shared
                            .ctx
                            .progress
                            .files_done
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => return,
                    Err(e) => {
                        if e.kind() == io::ErrorKind::StorageFull {
                            shared.disk_full.store(true, Ordering::Relaxed);
                        }
                        shared.fail(&item.rel, "コピー", &e);
                    }
                }
            });
        }
    });
}

/// Applies the plan. Deletions covered by `plan.guard` need `allow_guarded`.
pub fn execute(plan: Plan, ctx: &RunContext, allow_guarded: bool) -> Result<Report, MirrorError> {
    if let (Some(reason), false) = (&plan.guard, allow_guarded) {
        return Err(MirrorError::Guard(reason.clone()));
    }
    fs::create_dir_all(&plan.dest).map_err(|e| {
        MirrorError::Fatal(format!(
            "保存先を作成できません: {}: {e}",
            plan.dest.display()
        ))
    })?;
    if !plan.has_work() && matches!(plan.dest_scan, crate::plan::DestScan::Cached) {
        return Ok(Report {
            scan_errors: plan.scan_errors,
            ..Report::default()
        });
    }

    // From here until the clean finish, the cache is not trusted.
    let running = format!("running-{}", cache::new_run_id());
    cache::write_marker(&plan.dest, &running).map_err(|e| {
        MirrorError::Fatal(format!(
            "保存先に書き込めません: {}: {e}",
            plan.dest.display()
        ))
    })?;

    let progress = &ctx.progress;
    progress
        .files_total
        .store(plan.copies.len() as u64, Ordering::Relaxed);
    progress
        .bytes_total
        .store(plan.stats.copy_bytes, Ordering::Relaxed);
    progress
        .deletes_total
        .store(plan.delete_count() as u64, Ordering::Relaxed);
    progress.files_done.store(0, Ordering::Relaxed);
    progress.bytes_done.store(0, Ordering::Relaxed);
    progress.deletes_done.store(0, Ordering::Relaxed);
    progress.errors.store(0, Ordering::Relaxed);

    let shared = Shared {
        ctx,
        evict: Mutex::new(plan.evict.clone()),
        disk_full: AtomicBool::new(false),
        counter: AtomicU64::new(0),
    };

    // A file and folder that swapped places must be cleared before copying.
    remove_files(&plan.dest, &plan.conflict_files, &shared);
    remove_dirs(&plan.dest, &plan.conflict_dirs, &shared);

    let delete_first =
        platform::free_space(&plan.dest).is_some_and(|free| free < plan.stats.copy_bytes);
    if delete_first {
        ctx.log("空き容量が足りないため、先に削除します");
        progress.set_phase(Phase::Deleting);
        remove_files(&plan.dest, &plan.delete_files, &shared);
        remove_dirs(&plan.dest, &plan.delete_dirs, &shared);
    }

    progress.set_phase(Phase::Copying);
    for rel in &plan.mkdirs {
        if ctx.cancelled() {
            break;
        }
        if let Err(e) = fs::create_dir_all(plan.dest.join(rel)) {
            shared.fail(rel, "フォルダ作成", &e);
        }
    }
    copy_all(&plan, &shared);
    if shared.disk_full.load(Ordering::Relaxed) {
        ctx.log("保存先の空き容量がなくなったため、コピーを中断しました");
    }

    if !delete_first {
        progress.set_phase(Phase::Deleting);
        remove_files(&plan.dest, &plan.delete_files, &shared);
        remove_dirs(&plan.dest, &plan.delete_dirs, &shared);
    }
    progress.set_phase(Phase::Idle);

    if ctx.cancelled() {
        // The marker still says "running", so the next run lists the destination.
        return Err(MirrorError::Cancelled);
    }

    let errors = progress.errors.load(Ordering::Relaxed);
    let report = Report {
        copied: progress.files_done.load(Ordering::Relaxed),
        copied_bytes: progress.bytes_done.load(Ordering::Relaxed),
        deleted: progress.deletes_done.load(Ordering::Relaxed),
        errors,
        scan_errors: plan.scan_errors,
    };

    if plan.cache_enabled && !shared.disk_full.load(Ordering::Relaxed) {
        let mut dirs = plan.state;
        for rel in shared.evict.into_inner().unwrap() {
            dirs.remove(&rel);
        }
        let run_id = cache::new_run_id();
        let cache_file = CacheFile {
            run_id: run_id.clone(),
            volume_id: plan.volume_id,
            last_full: plan.last_full,
            dirs,
        };
        match cache::save(&plan.cache_file, &cache_file) {
            Ok(()) => {
                if let Err(e) = cache::write_marker(&plan.dest, &run_id) {
                    ctx.log(format!("状態を記録できませんでした: {e}"));
                }
            }
            Err(e) => ctx.log(format!("キャッシュを保存できませんでした: {e}")),
        }
    } else if !plan.cache_enabled {
        cache::discard(&plan.cache_file);
    }
    Ok(report)
}
