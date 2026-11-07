//! Compares a source and destination tree and produces the work to do.
//!
//! Each folder is listed on both sides by its own small thread pool, so a slow
//! disk does not hold back a fast one. A folder missing from the destination is
//! never listed there, and a folder held by the cache is not read from disk.

use crate::cache::{self, DirMap};
use crate::exclude::Excludes;
use crate::i18n::tr;
use crate::journal::{self, SourceIndex};
use crate::model::*;
use crate::platform;
use crate::trf;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Debug)]
pub struct CopyItem {
    pub rel: PathBuf,
    pub size: u64,
    pub is_new: bool,
    pub created: i64,
}

/// Metadata to apply to something already in the destination.
#[derive(Clone, Debug)]
pub struct MetaItem {
    pub rel: PathBuf,
    pub meta: platform::Meta,
}

#[derive(Default, Clone, Debug)]
pub struct PlanStats {
    pub source_files: u64,
    pub source_dirs: u64,
    pub dest_files: u64,
    pub unchanged: u64,
    pub copy_bytes: u64,
}

/// A link to recreate in the destination.
#[derive(Clone, Debug)]
pub struct LinkItem {
    pub rel: PathBuf,
    pub link: platform::LinkInfo,
}

/// How the destination was read for this plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestScan {
    /// Listed from disk. The text explains why the cache was not used.
    Full(String),
    Cached,
}

/// How the source was read for this plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceScan {
    /// Listed from disk, the normal way.
    Disk,
    /// The change journal was asked for, but this run listed from disk anyway.
    JournalUnused {
        reason: String,
        /// Administrator rights alone would have made the journal readable.
        denied: bool,
    },
    /// Only the folders named by the change journal were listed.
    Journal { changed_folders: u64 },
}

pub struct Plan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub copy_threads: usize,
    pub metadata_threads: usize,
    pub scan_threads: (usize, usize),
    pub dest_scan: DestScan,
    pub source_scan: SourceScan,
    pub copies: Vec<CopyItem>,
    pub mkdirs: Vec<PathBuf>,
    pub links: Vec<LinkItem>,
    /// Only the metadata of these items differs.
    pub metadata: Vec<MetaItem>,
    /// Folders this plan writes into or creates, whose dates are put back afterwards.
    pub restore_dirs: Vec<MetaItem>,
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
    /// Saved when the plan is carried out, for the next run's journal read.
    pub(crate) source_index: Option<SourceIndex>,
}

impl Plan {
    /// Where this pair's saved destination state is kept.
    pub fn state_file(&self) -> &Path {
        &self.cache_file
    }

    pub fn delete_count(&self) -> usize {
        self.delete_files.len() + self.conflict_files.len()
    }

    /// This plan's share of the overall progress bar.
    pub fn work_units(&self) -> u64 {
        crate::model::work_units(
            self.copies.len() as u64,
            self.stats.copy_bytes,
            (self.delete_count() + self.metadata.len() + self.restore_dirs.len()) as u64,
        )
    }

    pub fn has_work(&self) -> bool {
        !(self.copies.is_empty()
            && self.mkdirs.is_empty()
            && self.links.is_empty()
            && self.metadata.is_empty()
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
    links: Vec<LinkItem>,
    metadata: Vec<MetaItem>,
    dir_meta: HashMap<PathBuf, platform::Meta>,
    conflict_files: Vec<PathBuf>,
    conflict_dirs: Vec<PathBuf>,
    delete_files: Vec<PathBuf>,
    delete_dirs: Vec<PathBuf>,
    warnings: Vec<String>,
    stats: PlanStats,
    scan_errors: u64,
    state: DirMap,
    source_state: DirMap,
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
    source_saved: Mutex<Option<DirMap>>,
    keep_source: bool,
    copy_links: bool,
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

/// The metadata a destination folder should end up with.
fn dir_meta(s: &Entry) -> platform::Meta {
    platform::Meta {
        created: s.created,
        mtime: Some(s.mtime),
        attrs: Some(platform::dir_attrs(s.attrs)),
    }
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
        if slots.source.is_none() {
            let mut saved = self.source_saved.lock().unwrap();
            if let Some(listing) = saved.as_mut().and_then(|m| m.remove(&rel)) {
                self.ctx
                    .progress
                    .source_entries
                    .fetch_add(listing.len() as u64, Ordering::Relaxed);
                slots.source = Some(Ok(listing));
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
                    out.fatal = Some(trf!(
                        "Cannot read the folder: {error}",
                        "フォルダを読み取れません: {error}"
                    ));
                    self.ctx.cancel.store(true, Ordering::Relaxed);
                    return None;
                }
                out.scan_errors += 1;
                out.warnings.push(trf!(
                    "Skipped because it cannot be read: {shown}: {error}",
                    "読み取れないため処理しません: {shown}: {error}"
                ));
                if let Some(parent) = rel.parent() {
                    out.evict.insert(parent.to_path_buf());
                }
                out.evict.insert(rel.clone());
                return None;
            }
        };
        if self.keep_source && !matches!(node.mode, Mode::DestOnly { .. }) {
            let kept = source.clone();
            self.out
                .lock()
                .unwrap()
                .source_state
                .insert(rel.clone(), kept);
        }
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
                warnings.push(trf!(
                    "Ignored because another item has the same name: {}",
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

    fn created_differs(&self, s: &Entry, d: &Entry) -> bool {
        // Unix listings have no creation time to compare.
        s.created != 0 && d.created != 0 && (s.created - d.created).abs() > self.tolerance
    }

    fn count_source(&self, entry: &Entry, local: &mut Output) {
        match entry.kind {
            Kind::Dir => local.stats.source_dirs += 1,
            Kind::File => local.stats.source_files += 1,
            // A link being copied is content, so its source is not "empty".
            Kind::Link | Kind::DirLink if self.copy_links => local.stats.source_files += 1,
            _ => {}
        }
    }

    /// Reads a source link. A special file that is no link gets a warning.
    fn source_link(
        &self,
        rel: &Path,
        name: &std::ffi::OsStr,
        local: &mut Output,
    ) -> Option<platform::LinkInfo> {
        let path = rel.join(name);
        match platform::read_link(&join(self.source, &path)) {
            Ok(link) => Some(link),
            Err(e) => {
                local.warnings.push(trf!(
                    "This link cannot be copied: {}: {e}",
                    "このリンクはコピーできません: {}: {e}",
                    rel_text(&path)
                ));
                None
            }
        }
    }

    /// With links being copied: makes the destination's item match the source.
    fn both_with_links(
        &self,
        rel: &Path,
        s: &Entry,
        d: &Entry,
        local: &mut Output,
        state: &mut Vec<Entry>,
        children: &mut Vec<(PathBuf, Mode)>,
    ) {
        let path = rel.join(&s.name);
        if !s.kind.is_link() {
            // A real item replaces a destination link. Removing the link first
            // means nothing is written through it.
            match d.kind {
                Kind::DirLink => local.conflict_dirs.push(path.clone()),
                _ => local.conflict_files.push(path.clone()),
            }
            if s.kind == Kind::Dir {
                local.dir_meta.insert(path.clone(), dir_meta(s));
                local.mkdirs.push(path.clone());
                children.push((path, Mode::SourceOnly));
            } else {
                local.stats.copy_bytes += s.size;
                local.copies.push(CopyItem {
                    rel: path,
                    size: s.size,
                    is_new: true,
                    created: s.created,
                });
            }
            state.push(s.clone());
            return;
        }
        let Some(link) = self.source_link(rel, &s.name, local) else {
            state.push(d.clone());
            return;
        };
        match d.kind {
            Kind::Link | Kind::DirLink => match platform::read_link(&join(self.dest, &path)) {
                Ok(existing) if existing == link => {
                    local.stats.unchanged += 1;
                    state.push(s.clone());
                    return;
                }
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    local.warnings.push(trf!(
                        "Skipped because the destination item cannot be read as a link: {}: {e}",
                        "保存先をリンクとして読み取れないため処理しません: {}: {e}",
                        rel_text(&path)
                    ));
                    state.push(d.clone());
                    return;
                }
                _ if d.kind == Kind::DirLink => local.conflict_dirs.push(path.clone()),
                _ => local.conflict_files.push(path.clone()),
            },
            Kind::File => local.conflict_files.push(path.clone()),
            Kind::Dir => {
                local.conflict_dirs.push(path.clone());
                children.push((path.clone(), Mode::DestOnly { conflict: true }));
            }
        }
        local.links.push(LinkItem { rel: path, link });
        state.push(s.clone());
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
                    created: s.created,
                });
                state.push(s.clone());
            }
            Kind::Dir => {
                local.dir_meta.insert(path.clone(), dir_meta(s));
                local.mkdirs.push(path.clone());
                children.push((path, Mode::SourceOnly));
                state.push(s.clone());
            }
            Kind::Link | Kind::DirLink if self.copy_links => {
                if let Some(link) = self.source_link(rel, &s.name, local) {
                    local.links.push(LinkItem { rel: path, link });
                    state.push(s.clone());
                }
            }
            Kind::Link | Kind::DirLink => local.warnings.push(trf!(
                "Links are not copied: {}",
                "リンクはコピーしません: {}",
                rel_text(&path)
            )),
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
        if self.copy_links && (s.kind.is_link() || d.kind.is_link()) {
            self.both_with_links(rel, s, d, local, state, children);
            return;
        }
        if s.kind.is_link() || d.kind.is_link() {
            // Links are never followed or replaced: writing through one could
            // reach data outside the mirrored folder.
            local.warnings.push(trf!(
                "Skipped because it is a link: {}",
                "リンクがあるため処理しません: {}",
                rel_text(&path)
            ));
            state.push(d.clone());
            return;
        }
        match (s.kind, d.kind) {
            (Kind::Dir, Kind::Dir) => {
                let mut s = s.clone();
                if (s.mtime - d.mtime).abs() > self.tolerance || s.attrs != d.attrs {
                    // The listing can lag, so a difference is checked at the folder.
                    if let Some(actual) = platform::actual_dir_meta(&join(self.source, &path)) {
                        (s.mtime, s.attrs) = actual;
                    }
                }
                let differs = (s.mtime - d.mtime).abs() > self.tolerance
                    || self.created_differs(&s, d)
                    || platform::dir_attrs(s.attrs) != platform::dir_attrs(d.attrs);
                if differs {
                    local.metadata.push(MetaItem {
                        rel: path.clone(),
                        meta: dir_meta(&s),
                    });
                }
                local.dir_meta.insert(path.clone(), dir_meta(&s));
                state.push(s);
                children.push((path, Mode::Both));
            }
            (Kind::File, Kind::File) => {
                let same = s.size == d.size && (s.mtime - d.mtime).abs() <= self.tolerance;
                if same {
                    local.stats.unchanged += 1;
                    if s.attrs != d.attrs || self.created_differs(s, d) {
                        local.metadata.push(MetaItem {
                            rel: path,
                            meta: platform::Meta {
                                created: s.created,
                                mtime: None,
                                attrs: Some(s.attrs),
                            },
                        });
                    }
                } else {
                    local.stats.copy_bytes += s.size;
                    local.copies.push(CopyItem {
                        rel: path,
                        size: s.size,
                        is_new: false,
                        created: s.created,
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
                    created: s.created,
                });
                state.push(s.clone());
            }
            _ => {
                local.dir_meta.insert(path.clone(), dir_meta(s));
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
        out.links.extend(local.links);
        out.metadata.extend(local.metadata);
        out.dir_meta.extend(local.dir_meta);
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
    let absolute = std::path::absolute(dest).map_err(|e| {
        MirrorError::Fatal(trf!(
            "Invalid destination path: {e}",
            "保存先のパスが不正です: {e}"
        ))
    })?;
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
                    return Err(MirrorError::Fatal(trf!(
                        "The destination drive was not found: {}",
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
        return Err(tr("full check requested", "フルチェックが指定されました").into());
    }
    if options.full_check_days == 0 {
        return Err(tr("cache is turned off", "キャッシュは無効です").into());
    }
    if !dest.is_dir() {
        return Err(tr(
            "the destination does not exist yet",
            "保存先がまだありません",
        )
        .into());
    }
    let Some(loaded) = cache::load(cache_file) else {
        return Err(tr("no saved state yet", "キャッシュがありません").into());
    };
    if loaded.volume_id != volume_id {
        return Err(tr(
            "the destination disk changed",
            "保存先のディスクが前回と異なります",
        )
        .into());
    }
    if cache::read_marker(dest).as_deref() != Some(loaded.run_id.as_str()) {
        return Err(tr(
            "the last run did not finish, or the destination changed",
            "前回の実行が完了していないか、保存先が変更されました",
        )
        .into());
    }
    let age = cache::now_unix().saturating_sub(loaded.last_full);
    if age > u64::from(options.full_check_days) * 86_400 {
        return Err(tr("periodic full check is due", "定期フルチェックの時期です").into());
    }
    Ok(loaded)
}

/// Scans one folder pair. Nothing on disk changes.
pub fn scan(spec: &PairSpec, options: &ScanOptions, ctx: &RunContext) -> Result<Plan, MirrorError> {
    ctx.progress.set_phase(Phase::Scanning);
    let source = spec.source.canonicalize().map_err(|e| {
        MirrorError::Fatal(trf!(
            "The source folder was not found: {}: {e}",
            "元フォルダが見つかりません: {}: {e}",
            spec.source.display()
        ))
    })?;
    if !source.is_dir() {
        return Err(MirrorError::Fatal(trf!(
            "The source is not a folder: {}",
            "元がフォルダではありません: {}",
            spec.source.display()
        )));
    }
    let dest = resolve_dest(&spec.dest)?;
    let (a, b) = (comparable(&source), comparable(&dest));
    if a.starts_with(&b) || b.starts_with(&a) {
        return Err(MirrorError::Guard(trf!(
            "The source and destination overlap: {} / {}",
            "元と保存先が重なっています: {} / {}",
            display_path(&source),
            display_path(&dest)
        )));
    }
    if dest.exists() && !dest.is_dir() {
        return Err(MirrorError::Fatal(trf!(
            "The destination is not a folder: {}",
            "保存先がフォルダではありません: {}",
            display_path(&dest)
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
    // Metadata lives on the destination alone, so the source's disk has no say.
    let metadata_threads = options
        .copy_threads
        .unwrap_or(dest_disk.copy_threads())
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

    // The journal is only worth trusting along with the rest of the saved state,
    // so a full check of the destination also lists the whole source.
    let prepared = options.use_journal.then(|| {
        let mut prepared = journal::prepare(
            &source,
            &journal::index_path(&cache_file),
            options.journal_remote.as_ref(),
        );
        if let DestScan::Full(reason) = &dest_scan {
            if prepared.saved.take().is_some() {
                prepared.outcome = Err(reason.clone());
            }
        }
        prepared
    });
    if !options.use_journal {
        journal::discard_index(&cache_file);
    }
    let (source_saved, source_scan, next_index, unchanged_since) = match prepared {
        None => (None, SourceScan::Disk, None, None),
        Some(journal::Prepared {
            saved,
            outcome,
            denied,
            next,
            unchanged_since,
        }) => {
            let scan = match outcome {
                Ok(changed_folders) => SourceScan::Journal { changed_folders },
                Err(reason) => SourceScan::JournalUnused { reason, denied },
            };
            (saved, scan, next, unchanged_since)
        }
    };

    let walker = Walker {
        source: &source,
        dest: &dest,
        excludes: &excludes,
        tolerance,
        ctx,
        cached: Mutex::new(cached),
        source_saved: Mutex::new(source_saved),
        keep_source: next_index.is_some(),
        copy_links: options.copy_links,
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
    out.links.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    out.delete_files.sort_unstable();
    let deepest_first = |a: &PathBuf, b: &PathBuf| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    };
    out.delete_dirs.sort_unstable_by(deepest_first);
    out.conflict_dirs.sort_unstable_by(deepest_first);

    out.metadata.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    // Writing into a folder changes its date, so every folder touched gets its
    // date back afterwards, as does every folder created.
    let shown: HashSet<&PathBuf> = out.metadata.iter().map(|m| &m.rel).collect();
    let parent_of = |rel: &PathBuf| rel.parent().map(Path::to_path_buf);
    let touched: HashSet<PathBuf> = out
        .copies
        .iter()
        .map(|c| &c.rel)
        .chain(out.links.iter().map(|l| &l.rel))
        .chain(&out.delete_files)
        .chain(&out.delete_dirs)
        .chain(&out.conflict_files)
        .chain(&out.conflict_dirs)
        .chain(&out.mkdirs)
        .filter_map(parent_of)
        .chain(out.mkdirs.iter().cloned())
        .collect();
    let mut restore_dirs: Vec<MetaItem> = touched
        .into_iter()
        .filter(|rel| !shown.contains(rel))
        .filter_map(|rel| {
            let mut meta = *out.dir_meta.get(&rel)?;
            // The listing can lag for a folder whose contents just changed.
            if let Some((mtime, attrs)) = platform::actual_dir_meta(&source.join(&rel)) {
                meta.mtime = Some(mtime);
                meta.attrs = Some(platform::dir_attrs(attrs));
                let entries = rel.parent().and_then(|parent| out.state.get_mut(parent));
                let found = entries.and_then(|entries| {
                    let name = rel.file_name()?;
                    entries.iter_mut().find(|e| e.name == name)
                });
                if let Some(entry) = found {
                    entry.mtime = mtime;
                    entry.attrs = attrs;
                }
            }
            Some(MetaItem { rel, meta })
        })
        .collect();
    restore_dirs.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));

    let delete_total = (out.delete_files.len() + out.conflict_files.len()) as u64;
    let delete_dirs_total = (out.delete_dirs.len() + out.conflict_dirs.len()) as u64;
    let updates = out.copies.iter().filter(|c| !c.is_new).count() as u64;
    let source_empty = out.stats.source_files == 0 && out.stats.source_dirs == 0;
    let guard = if source_empty && (delete_total > 0 || delete_dirs_total > 0) {
        Some(trf!(
            "The source folder is empty, but {delete_total} files would be deleted from the destination. Check that the drive is connected.",
            "元フォルダが空なのに、保存先のファイルを {delete_total} 件削除しようとしています。ドライブの接続を確認してください"
        ))
    } else if dest_existed && !had_marker && (delete_total > 0 || delete_dirs_total > 0) {
        Some(trf!(
            "{delete_total} files would be deleted from a destination that Hayakagami has never mirrored to. Check that this is the right folder.",
            "このソフトでミラーしたことがない保存先のファイルを {delete_total} 件削除しようとしています。保存先が正しいか確認してください"
        ))
    } else if dest_existed && !had_marker && updates > 0 {
        Some(trf!(
            "{updates} existing files would be overwritten in a destination that Hayakagami has never mirrored to. Check that this is the right folder.",
            "このソフトでミラーしたことがない保存先のファイルを {updates} 件上書きしようとしています。保存先が正しいか確認してください"
        ))
    } else if out.stats.dest_files >= 10 && updates * 10 >= out.stats.dest_files {
        Some(trf!(
            "{updates} of the {} files in the destination would be overwritten. Check that the source files are not damaged, by ransomware for example.",
            "保存先の {} 件のうち {updates} 件を上書きしようとしています。ランサムウェアなどで元のファイルが壊れていないか確認してください",
            out.stats.dest_files
        ))
    } else if out.stats.dest_files >= 10 && delete_total * 2 >= out.stats.dest_files {
        Some(trf!(
            "{delete_total} of the {} files in the destination would be deleted. Check that the source and destination are right.",
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
        metadata_threads,
        scan_threads: (source_threads, dest_threads),
        dest_scan,
        source_scan,
        copies: out.copies,
        mkdirs: out.mkdirs,
        links: out.links,
        metadata: out.metadata,
        restore_dirs,
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
        source_index: next_index.map(|(cursor, root, volume_id)| SourceIndex {
            cursor,
            root,
            volume_id,
            dirs: out.source_state,
            unchanged_since,
        }),
    })
}
