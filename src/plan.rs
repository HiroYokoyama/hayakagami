//! Compares a source and destination tree and produces the work to do.
//!
//! Each folder is listed on both sides by its own small thread pool, so a slow
//! disk does not hold back a fast one. A folder missing from the destination is
//! never listed there, and a folder held by the cache is not read from disk.

use crate::cache::{self, DirMap};
use crate::exclude::Excludes;
use crate::model::*;
use crate::platform;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Debug)]
pub struct CopyItem {
    pub rel: PathBuf,
    pub size: u64,
    pub is_new: bool,
}

#[derive(Default, Clone, Debug)]
pub struct PlanStats {
    pub source_files: u64,
    pub source_dirs: u64,
    pub dest_files: u64,
    pub unchanged: u64,
    pub copy_bytes: u64,
}

/// How the destination was read for this plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestScan {
    /// Listed from disk. The text explains why the cache was not used.
    Full(String),
    Cached,
}

pub struct Plan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub copy_threads: usize,
    pub scan_threads: (usize, usize),
    pub dest_scan: DestScan,
    pub copies: Vec<CopyItem>,
    pub mkdirs: Vec<PathBuf>,
    /// Removed before copying because a file and a folder swapped places.
    pub conflict_files: Vec<PathBuf>,
    pub conflict_dirs: Vec<PathBuf>,
    pub delete_files: Vec<PathBuf>,
    pub delete_dirs: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub stats: PlanStats,
    /// The reason deletion needs explicit approval, if any.
    pub guard: Option<String>,
    pub scan_errors: u64,
    pub(crate) state: DirMap,
    pub(crate) evict: HashSet<PathBuf>,
    pub(crate) volume_id: String,
    pub(crate) last_full: u64,
    pub(crate) cache_file: PathBuf,
    pub(crate) cache_enabled: bool,
}

impl Plan {
    pub fn delete_count(&self) -> usize {
        self.delete_files.len() + self.conflict_files.len()
    }

    pub fn has_work(&self) -> bool {
        !(self.copies.is_empty()
            && self.mkdirs.is_empty()
            && self.conflict_files.is_empty()
            && self.conflict_dirs.is_empty()
            && self.delete_files.is_empty()
            && self.delete_dirs.is_empty())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Both,
    SourceOnly,
    /// Destination tree without a source: enumerate it for removal.
    DestOnly {
        conflict: bool,
    },
}

type Listing = io::Result<Vec<Entry>>;

#[derive(Default)]
struct Slots {
    source: Option<Listing>,
    dest: Option<Listing>,
}

struct Node {
    rel: PathBuf,
    mode: Mode,
    slots: Mutex<Slots>,
}

enum Task {
    ListSource(Arc<Node>),
    ListDest(Arc<Node>),
    Process(Arc<Node>),
}

#[derive(Default)]
struct Queues {
    source: Vec<Task>,
    dest: Vec<Task>,
    /// Queued plus running tasks. Zero means the walk has finished.
    outstanding: usize,
}

#[derive(Default)]
struct Output {
    copies: Vec<CopyItem>,
    mkdirs: Vec<PathBuf>,
    conflict_files: Vec<PathBuf>,
    conflict_dirs: Vec<PathBuf>,
    delete_files: Vec<PathBuf>,
    delete_dirs: Vec<PathBuf>,
    warnings: Vec<String>,
    stats: PlanStats,
    scan_errors: u64,
    state: DirMap,
    evict: HashSet<PathBuf>,
    fatal: Option<String>,
}

struct Walker<'a> {
    source: &'a Path,
    dest: &'a Path,
    excludes: &'a Excludes,
    tolerance: i64,
    ctx: &'a RunContext,
    cached: Mutex<Option<DirMap>>,
    queues: Mutex<Queues>,
    wake: Condvar,
    out: Mutex<Output>,
}

fn join(root: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn rel_text(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

fn is_marker(rel: &Path, name: &std::ffi::OsStr) -> bool {
    rel.as_os_str().is_empty() && name == MARKER_NAME
}

fn is_tmp(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().ends_with(TMP_SUFFIX)
}

impl Walker<'_> {
    fn submit(&self, rel: PathBuf, mode: Mode) {
        let mut slots = Slots::default();
        if mode == Mode::SourceOnly {
            slots.dest = Some(Ok(Vec::new()));
        }
        if matches!(mode, Mode::DestOnly { .. }) {
            slots.source = Some(Ok(Vec::new()));
        }
        if slots.dest.is_none() {
            let mut cached = self.cached.lock().unwrap();
            if let Some(listing) = cached.as_mut().and_then(|m| m.remove(&rel)) {
                self.ctx
                    .progress
                    .dest_entries
                    .fetch_add(listing.len() as u64, Ordering::Relaxed);
                slots.dest = Some(Ok(listing));
            }
        }
        let need_source = slots.source.is_none();
        let need_dest = slots.dest.is_none();
        let node = Arc::new(Node {
            rel,
            mode,
            slots: Mutex::new(slots),
        });
        let mut queues = self.queues.lock().unwrap();
        if need_source {
            queues.source.push(Task::ListSource(node.clone()));
            queues.outstanding += 1;
        }
        if need_dest {
            queues.dest.push(Task::ListDest(node.clone()));
            queues.outstanding += 1;
        }
        if !need_source && !need_dest {
            queues.source.push(Task::Process(node));
            queues.outstanding += 1;
        }
        drop(queues);
        self.wake.notify_all();
    }

    fn worker(&self, source_side: bool) {
        loop {
            let task = {
                let mut queues = self.queues.lock().unwrap();
                loop {
                    if self.ctx.cancelled() {
                        queues.source.clear();
                        queues.dest.clear();
                        queues.outstanding = 0;
                        self.wake.notify_all();
                        return;
                    }
                    let side = if source_side {
                        &mut queues.source
                    } else {
                        &mut queues.dest
                    };
                    if let Some(task) = side.pop() {
                        break task;
                    }
                    if queues.outstanding == 0 {
                        return;
                    }
                    queues = self.wake.wait(queues).unwrap();
                }
            };
            self.run_task(task);
            let mut queues = self.queues.lock().unwrap();
            queues.outstanding = queues.outstanding.saturating_sub(1);
            if queues.outstanding == 0 {
                self.wake.notify_all();
            }
        }
    }

    fn run_task(&self, task: Task) {
        match task {
            Task::ListSource(node) => {
                let listing = platform::list_dir(&join(self.source, &node.rel));
                if let Ok(items) = &listing {
                    self.ctx
                        .progress
                        .source_entries
                        .fetch_add(items.len() as u64, Ordering::Relaxed);
                }
                self.deliver(node, Some(listing), None);
            }
            Task::ListDest(node) => {
                let listing = platform::list_dir(&join(self.dest, &node.rel));
                if let Ok(items) = &listing {
                    self.ctx
                        .progress
                        .dest_entries
                        .fetch_add(items.len() as u64, Ordering::Relaxed);
                }
                self.deliver(node, None, Some(listing));
            }
            Task::Process(node) => self.deliver(node, None, None),
        }
    }

    fn deliver(&self, node: Arc<Node>, source: Option<Listing>, dest: Option<Listing>) {
        let ready = {
            let mut slots = node.slots.lock().unwrap();
            if source.is_some() {
                slots.source = source;
            }
            if dest.is_some() {
                slots.dest = dest;
            }
            match (slots.source.take(), slots.dest.take()) {
                (Some(source), Some(dest)) => Some((source, dest)),
                (source, dest) => {
                    slots.source = source;
                    slots.dest = dest;
                    None
                }
            }
        };
        if let Some((source, dest)) = ready {
            self.process(&node, source, dest);
        }
    }

    fn excluded(&self, rel: &Path, name: &std::ffi::OsStr, is_dir: bool) -> bool {
        if self.excludes.is_empty() {
            return false;
        }
        self.excludes.matches(&rel_text(&rel.join(name)), is_dir)
    }

    fn process(&self, node: &Node, source: Listing, dest: Listing) -> Option<()> {
        let rel = &node.rel;
        let (source, dest) = match (source, dest) {
            (Ok(s), Ok(d)) => (s, d),
            (s, d) => {
                // Nothing is known about this folder, so nothing in it may be removed.
                let error = s.err().or(d.err()).map_or(String::new(), |e| e.to_string());
                let shown = rel_text(rel);
                let mut out = self.out.lock().unwrap();
                if rel.as_os_str().is_empty() {
                    out.fatal = Some(format!("フォルダを読み取れません: {error}"));
                    self.ctx.cancel.store(true, Ordering::Relaxed);
                    return None;
                }
                out.scan_errors += 1;
                out.warnings
                    .push(format!("読み取れないため処理しません: {shown}: {error}"));
                if let Some(parent) = rel.parent() {
                    out.evict.insert(parent.to_path_buf());
                }
                out.evict.insert(rel.clone());
                return None;
            }
        };
        if let Mode::DestOnly { conflict } = node.mode {
            self.process_dest_only(rel, dest, conflict);
        } else {
            self.process_pair(rel, source, dest);
        }
        Some(())
    }

    fn process_dest_only(&self, rel: &Path, dest: Vec<Entry>, conflict: bool) {
        let mut children = Vec::new();
        let mut out = self.out.lock().unwrap();
        for e in dest {
            let is_dir = e.kind == Kind::Dir;
            let path = rel.join(&e.name);
            let tmp = is_tmp(&e.name);
            if !tmp && self.excluded(rel, &e.name, is_dir || e.kind == Kind::DirLink) {
                // Keep it: the parent folder's removal will then stop harmlessly.
                continue;
            }
            match e.kind {
                Kind::Dir => {
                    if conflict {
                        out.conflict_dirs.push(path.clone());
                    } else {
                        out.delete_dirs.push(path.clone());
                    }
                    children.push(path);
                }
                Kind::DirLink => {
                    out.stats.dest_files += 1;
                    if conflict {
                        out.conflict_dirs.push(path);
                    } else {
                        out.delete_dirs.push(path);
                    }
                }
                Kind::File | Kind::Link => {
                    out.stats.dest_files += 1;
                    if conflict {
                        out.conflict_files.push(path);
                    } else {
                        out.delete_files.push(path);
                    }
                }
            }
        }
        drop(out);
        for child in children.into_iter().rev() {
            self.submit(child, Mode::DestOnly { conflict });
        }
    }

    fn sorted(
        &self,
        rel: &Path,
        items: Vec<Entry>,
        warnings: &mut Vec<String>,
    ) -> Vec<(String, Entry)> {
        let mut keyed: Vec<(String, Entry)> =
            items.into_iter().map(|e| (name_key(&e.name), e)).collect();
        keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        keyed.dedup_by(|later, first| {
            let same = later.0 == first.0;
            if same {
                warnings.push(format!(
                    "同じ名前として扱われるため無視します: {}",
                    rel_text(&rel.join(&later.1.name))
                ));
            }
            same
        });
        keyed
    }

    fn process_pair(&self, rel: &Path, source: Vec<Entry>, dest: Vec<Entry>) {
        let mut local = Output::default();
        let mut children: Vec<(PathBuf, Mode)> = Vec::new();
        let source = self.sorted(rel, source, &mut local.warnings);
        let dest = self.sorted(rel, dest, &mut local.warnings);
        let mut final_state: Vec<Entry> = Vec::with_capacity(source.len());
        let (mut si, mut di) = (0, 0);
        while si < source.len() || di < dest.len() {
            let order = match (source.get(si), dest.get(di)) {
                (Some(s), Some(d)) => s.0.cmp(&d.0),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match order {
                std::cmp::Ordering::Less => {
                    self.source_only(
                        rel,
                        &source[si].1,
                        &mut local,
                        &mut final_state,
                        &mut children,
                    );
                    si += 1;
                }
                std::cmp::Ordering::Greater => {
                    self.dest_only(
                        rel,
                        &dest[di].1,
                        &mut local,
                        &mut final_state,
                        &mut children,
                    );
                    di += 1;
                }
                std::cmp::Ordering::Equal => {
                    self.both(
                        rel,
                        &source[si].1,
                        &dest[di].1,
                        &mut local,
                        &mut final_state,
                        &mut children,
                    );
                    si += 1;
                    di += 1;
                }
            }
        }
        local.state.insert(rel.to_path_buf(), final_state);
        self.merge(local);
        for (child, mode) in children.into_iter().rev() {
            self.submit(child, mode);
        }
    }

    fn count_source(&self, entry: &Entry, local: &mut Output) {
        match entry.kind {
            Kind::Dir => local.stats.source_dirs += 1,
            Kind::File => local.stats.source_files += 1,
            _ => {}
        }
    }

    fn source_only(
        &self,
        rel: &Path,
        s: &Entry,
        local: &mut Output,
        state: &mut Vec<Entry>,
        children: &mut Vec<(PathBuf, Mode)>,
    ) {
        if is_marker(rel, &s.name) {
            return;
        }
        let is_dir = s.kind == Kind::Dir;
        if self.excluded(rel, &s.name, is_dir) {
            return;
        }
        self.count_source(s, local);
        let path = rel.join(&s.name);
        match s.kind {
            Kind::File => {
                local.stats.copy_bytes += s.size;
                local.copies.push(CopyItem {
                    rel: path,
                    size: s.size,
                    is_new: true,
                });
                state.push(s.clone());
            }
            Kind::Dir => {
                local.mkdirs.push(path.clone());
                children.push((path, Mode::SourceOnly));
                state.push(s.clone());
            }
            Kind::Link | Kind::DirLink => local
                .warnings
                .push(format!("リンクはコピーしません: {}", rel_text(&path))),
        }
    }

    fn dest_only(
        &self,
        rel: &Path,
        d: &Entry,
        local: &mut Output,
        state: &mut Vec<Entry>,
        children: &mut Vec<(PathBuf, Mode)>,
    ) {
        if is_marker(rel, &d.name) {
            return;
        }
        let path = rel.join(&d.name);
        if d.kind != Kind::Dir {
            local.stats.dest_files += 1;
        }
        let tmp = is_tmp(&d.name);
        if !tmp && self.excluded(rel, &d.name, matches!(d.kind, Kind::Dir | Kind::DirLink)) {
            state.push(d.clone());
            return;
        }
        match d.kind {
            Kind::Dir => {
                local.delete_dirs.push(path.clone());
                children.push((path, Mode::DestOnly { conflict: false }));
            }
            Kind::DirLink => local.delete_dirs.push(path),
            Kind::File | Kind::Link => local.delete_files.push(path),
        }
    }

    fn both(
        &self,
        rel: &Path,
        s: &Entry,
        d: &Entry,
        local: &mut Output,
        state: &mut Vec<Entry>,
        children: &mut Vec<(PathBuf, Mode)>,
    ) {
        if is_marker(rel, &s.name) {
            return;
        }
        let path = rel.join(&s.name);
        if d.kind != Kind::Dir {
            local.stats.dest_files += 1;
        }
        if self.excluded(rel, &s.name, s.kind == Kind::Dir) {
            state.push(d.clone());
            return;
        }
        self.count_source(s, local);
        if s.kind.is_link() || d.kind.is_link() {
            // Links are never followed or replaced: writing through one could
            // reach data outside the mirrored folder.
            local
                .warnings
                .push(format!("リンクがあるため処理しません: {}", rel_text(&path)));
            state.push(d.clone());
            return;
        }
        match (s.kind, d.kind) {
            (Kind::Dir, Kind::Dir) => {
                state.push(s.clone());
                children.push((path, Mode::Both));
            }
            (Kind::File, Kind::File) => {
                let same = s.size == d.size && (s.mtime - d.mtime).abs() <= self.tolerance;
                if same {
                    local.stats.unchanged += 1;
                } else {
                    local.stats.copy_bytes += s.size;
                    local.copies.push(CopyItem {
                        rel: path,
                        size: s.size,
                        is_new: false,
                    });
                }
                state.push(s.clone());
            }
            (Kind::File, _) => {
                local.conflict_dirs.push(path.clone());
                children.push((path.clone(), Mode::DestOnly { conflict: true }));
                local.stats.copy_bytes += s.size;
                local.copies.push(CopyItem {
                    rel: path,
                    size: s.size,
                    is_new: true,
                });
                state.push(s.clone());
            }
            _ => {
                local.conflict_files.push(path.clone());
                local.mkdirs.push(path.clone());
                children.push((path, Mode::SourceOnly));
                state.push(s.clone());
            }
        }
    }

    fn merge(&self, local: Output) {
        let mut out = self.out.lock().unwrap();
        out.copies.extend(local.copies);
        out.mkdirs.extend(local.mkdirs);
        out.conflict_files.extend(local.conflict_files);
        out.conflict_dirs.extend(local.conflict_dirs);
        out.delete_files.extend(local.delete_files);
        out.delete_dirs.extend(local.delete_dirs);
        out.warnings.extend(local.warnings);
        out.state.extend(local.state);
        out.stats.source_files += local.stats.source_files;
        out.stats.source_dirs += local.stats.source_dirs;
        out.stats.dest_files += local.stats.dest_files;
        out.stats.unchanged += local.stats.unchanged;
        out.stats.copy_bytes += local.stats.copy_bytes;
    }
}

/// Resolves a destination that may not exist yet through its nearest existing ancestor.
fn resolve_dest(dest: &Path) -> Result<PathBuf, MirrorError> {
    let absolute = std::path::absolute(dest)
        .map_err(|e| MirrorError::Fatal(format!("保存先のパスが不正です: {e}")))?;
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for part in rest.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(_) => {
                let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
                    return Err(MirrorError::Fatal(format!(
                        "保存先のドライブが見つかりません: {}",
                        dest.display()
                    )));
                };
                rest.push(name.to_os_string());
                existing = parent;
            }
        }
    }
}

fn comparable(path: &Path) -> PathBuf {
    if cfg!(any(windows, target_os = "macos")) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// Decides whether the remembered destination listing can replace a disk scan.
fn usable_cache(
    dest: &Path,
    options: &ScanOptions,
    volume_id: &str,
    cache_file: &Path,
) -> Result<cache::CacheFile, String> {
    if options.force_full {
        return Err("フルチェックが指定されました".into());
    }
    if options.full_check_days == 0 {
        return Err("キャッシュは無効です".into());
    }
    if !dest.is_dir() {
        return Err("保存先がまだありません".into());
    }
    let Some(loaded) = cache::load(cache_file) else {
        return Err("キャッシュがありません".into());
    };
    if loaded.volume_id != volume_id {
        return Err("保存先のディスクが前回と異なります".into());
    }
    if cache::read_marker(dest).as_deref() != Some(loaded.run_id.as_str()) {
        return Err("前回の実行が完了していないか、保存先が変更されました".into());
    }
    let age = cache::now_unix().saturating_sub(loaded.last_full);
    if age > u64::from(options.full_check_days) * 86_400 {
        return Err("定期フルチェックの時期です".into());
    }
    Ok(loaded)
}

/// Scans one folder pair. Nothing on disk changes.
pub fn scan(spec: &PairSpec, options: &ScanOptions, ctx: &RunContext) -> Result<Plan, MirrorError> {
    ctx.progress.set_phase(Phase::Scanning);
    let source = spec.source.canonicalize().map_err(|e| {
        MirrorError::Fatal(format!(
            "元フォルダが見つかりません: {}: {e}",
            spec.source.display()
        ))
    })?;
    if !source.is_dir() {
        return Err(MirrorError::Fatal(format!(
            "元がフォルダではありません: {}",
            spec.source.display()
        )));
    }
    let dest = resolve_dest(&spec.dest)?;
    let (a, b) = (comparable(&source), comparable(&dest));
    if a.starts_with(&b) || b.starts_with(&a) {
        return Err(MirrorError::Guard(format!(
            "元と保存先が重なっています: {} / {}",
            source.display(),
            dest.display()
        )));
    }
    if dest.exists() && !dest.is_dir() {
        return Err(MirrorError::Fatal(format!(
            "保存先がフォルダではありません: {}",
            dest.display()
        )));
    }
    let excludes = Excludes::new(&spec.excludes).map_err(MirrorError::Fatal)?;

    let dest_probe = {
        let mut probe = dest.as_path();
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            }
        }
        probe.to_path_buf()
    };
    let source_disk = platform::disk_kind(&source);
    let dest_disk = platform::disk_kind(&dest_probe);
    let source_threads = options
        .scan_threads
        .unwrap_or(source_disk.scan_threads())
        .max(1);
    let dest_threads = options
        .scan_threads
        .unwrap_or(dest_disk.scan_threads())
        .max(1);
    let copy_threads = options
        .copy_threads
        .unwrap_or(source_disk.copy_threads().min(dest_disk.copy_threads()))
        .max(1);
    let tolerance = platform::mtime_tolerance(&dest_probe);
    let volume_id = platform::volume_info(&dest_probe).map_or(String::new(), |v| v.0);
    let cache_file = cache::cache_path(&crate::config::cache_dir(), &source, &dest);
    let cache_enabled = options.full_check_days != 0;
    let dest_existed = dest.is_dir();
    let had_marker = dest_existed && cache::marker_path(&dest).exists();

    let (cached, dest_scan, last_full) = match usable_cache(&dest, options, &volume_id, &cache_file)
    {
        Ok(loaded) => (Some(loaded.dirs), DestScan::Cached, loaded.last_full),
        Err(reason) => (None, DestScan::Full(reason), cache::now_unix()),
    };

    let walker = Walker {
        source: &source,
        dest: &dest,
        excludes: &excludes,
        tolerance,
        ctx,
        cached: Mutex::new(cached),
        queues: Mutex::new(Queues::default()),
        wake: Condvar::new(),
        out: Mutex::new(Output::default()),
    };
    let root_mode = if dest_existed {
        Mode::Both
    } else {
        Mode::SourceOnly
    };
    walker.submit(PathBuf::new(), root_mode);
    std::thread::scope(|scope| {
        for _ in 0..source_threads {
            scope.spawn(|| walker.worker(true));
        }
        for _ in 0..dest_threads {
            scope.spawn(|| walker.worker(false));
        }
    });

    let mut out = walker.out.into_inner().unwrap();
    if let Some(fatal) = out.fatal.take() {
        return Err(MirrorError::Fatal(fatal));
    }
    if ctx.cancelled() {
        return Err(MirrorError::Cancelled);
    }
    out.copies.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    out.mkdirs.sort_unstable();
    out.delete_files.sort_unstable();
    let deepest_first = |a: &PathBuf, b: &PathBuf| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    };
    out.delete_dirs.sort_unstable_by(deepest_first);
    out.conflict_dirs.sort_unstable_by(deepest_first);

    let delete_total = (out.delete_files.len() + out.conflict_files.len()) as u64;
    let delete_dirs_total = (out.delete_dirs.len() + out.conflict_dirs.len()) as u64;
    let source_empty = out.stats.source_files == 0 && out.stats.source_dirs == 0;
    let guard = if source_empty && (delete_total > 0 || delete_dirs_total > 0) {
        Some(format!(
            "元フォルダが空なのに、保存先のファイルを {delete_total} 件削除しようとしています。ドライブの接続を確認してください"
        ))
    } else if dest_existed && !had_marker && (delete_total > 0 || delete_dirs_total > 0) {
        Some(format!(
            "このソフトでミラーしたことがない保存先のファイルを {delete_total} 件削除しようとしています。保存先が正しいか確認してください"
        ))
    } else if out.stats.dest_files >= 10 && delete_total * 2 >= out.stats.dest_files {
        Some(format!(
            "保存先の {} 件のうち {delete_total} 件を削除しようとしています。元と保存先が正しいか確認してください",
            out.stats.dest_files
        ))
    } else {
        None
    };

    Ok(Plan {
        source,
        dest,
        copy_threads,
        scan_threads: (source_threads, dest_threads),
        dest_scan,
        copies: out.copies,
        mkdirs: out.mkdirs,
        conflict_files: out.conflict_files,
        conflict_dirs: out.conflict_dirs,
        delete_files: out.delete_files,
        delete_dirs: out.delete_dirs,
        warnings: out.warnings,
        stats: out.stats,
        guard,
        scan_errors: out.scan_errors,
        state: out.state,
        evict: out.evict,
        volume_id,
        last_full,
        cache_file,
        cache_enabled,
    })
}
