#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Desktop interface.

use eframe::egui::{self, Color32, CornerRadius, Frame, RichText, Stroke};
use hayakagami::config::{self, Config, FolderPair, Pattern};
use hayakagami::elevate;
use hayakagami::exec;
use hayakagami::i18n::{self, tr, Lang};
use hayakagami::journal::{self, RemoteHandle};
use hayakagami::model::{display_path, MirrorError, PairSpec, Phase, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan, SourceScan};
use hayakagami::platform;
use hayakagami::trf;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(14, 116, 128);
const ACCENT_SOFT: Color32 = Color32::from_rgb(208, 236, 240);
const BACKGROUND: Color32 = Color32::from_rgb(244, 246, 248);
const CARD: Color32 = Color32::WHITE;
const BORDER: Color32 = Color32::from_rgb(222, 226, 231);
const TEXT: Color32 = Color32::from_rgb(33, 41, 52);
const MUTED: Color32 = Color32::from_rgb(103, 113, 126);
const GREEN: Color32 = Color32::from_rgb(22, 128, 80);
const BLUE: Color32 = Color32::from_rgb(35, 99, 190);
const RED: Color32 = Color32::from_rgb(196, 52, 44);
const AMBER: Color32 = Color32::from_rgb(170, 110, 10);
const RED_SOFT: Color32 = Color32::from_rgb(253, 236, 234);

#[derive(Clone, Copy)]
enum RowKind {
    New,
    Update,
    Delete,
    Warning,
}

impl RowKind {
    fn label(self) -> &'static str {
        match self {
            RowKind::New => tr("New", "新規"),
            RowKind::Update => tr("Update", "更新"),
            RowKind::Delete => tr("Delete", "削除"),
            RowKind::Warning => tr("Warning", "警告"),
        }
    }

    fn color(self) -> Color32 {
        match self {
            RowKind::New => GREEN,
            RowKind::Update => BLUE,
            RowKind::Delete => RED,
            RowKind::Warning => AMBER,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Run,
    Settings,
}

enum Message {
    Log(String),
    Plans(Vec<Plan>),
    Finished,
}

struct App {
    tab: Tab,
    config: Config,
    config_path: PathBuf,
    /// Set when the saved file could not be read, so it is never overwritten.
    load_error: Option<String>,
    selected: usize,
    excludes_text: String,
    dirty_since: Option<Instant>,
    full_check: bool,
    allow_guarded: bool,
    plans: Option<Vec<Plan>>,
    rows: Vec<(RowKind, String)>,
    log: Vec<String>,
    job: Option<(RunContext, Receiver<Message>)>,
    /// The pattern file this window edits. Saved patterns are left alone then.
    file_mode: Option<PathBuf>,
    /// What to start after opening a pattern file: true runs, false previews.
    auto_start: Option<bool>,
    /// The elevated change journal reader, kept for the life of this window.
    journal_remote: Option<RemoteHandle>,
    /// The permission prompt was declined, so it is not shown again in this window.
    journal_declined: bool,
}

fn load_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
            y_offset_factor: 0.12,
            ..Default::default()
        })),
    );
    for (family, position) in [
        (egui::FontFamily::Proportional, 0),
        (egui::FontFamily::Monospace, 1),
    ] {
        let list = fonts.families.entry(family).or_default();
        let at = position.min(list.len());
        list.insert(at, "cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = Color32::from_rgb(249, 250, 251);
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    let radius = CornerRadius::same(6);
    for state in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        state.corner_radius = radius;
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.weak_bg_fill = CARD;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(232, 235, 239);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(238, 241, 244);
    ctx.set_visuals_of(egui::Theme::Light, visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.interact_size.y = 28.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
    });
}

fn card() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(12)
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(MUTED).size(12.5).strong());
}

fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(text.to_owned())
            .color(Color32::WHITE)
            .strong(),
    )
    .fill(ACCENT)
    .stroke(Stroke::NONE)
}

fn tile(ui: &mut egui::Ui, label: &str, value: String, detail: Option<String>, color: Color32) {
    card().show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(label).color(MUTED).size(12.0));
        ui.label(RichText::new(value).color(color).size(22.0).strong());
        ui.add(
            egui::Label::new(
                RichText::new(detail.unwrap_or_else(|| " ".to_owned()))
                    .color(MUTED)
                    .size(12.0),
            )
            .truncate(),
        );
    });
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn short_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| display_path(path), |n| n.to_string_lossy().into_owned())
}

fn pair_label(plan: &Plan) -> String {
    format!("{} → {}", short_name(&plan.source), short_name(&plan.dest))
}

fn build_rows(plans: &[Plan]) -> Vec<(RowKind, String)> {
    let mut rows = Vec::new();
    for plan in plans {
        for item in &plan.copies {
            let kind = if item.is_new {
                RowKind::New
            } else {
                RowKind::Update
            };
            rows.push((kind, item.rel.display().to_string()));
        }
        for item in &plan.metadata {
            rows.push((
                RowKind::Update,
                trf!(
                    "{} (metadata only)",
                    "{}(メタデータのみ)",
                    item.rel.display()
                ),
            ));
        }
        for item in &plan.links {
            rows.push((
                RowKind::New,
                format!("{} -> {}", item.rel.display(), item.link.target.display()),
            ));
        }
        for rel in plan.conflict_files.iter().chain(&plan.delete_files) {
            rows.push((RowKind::Delete, rel.display().to_string()));
        }
        for rel in plan.conflict_dirs.iter().chain(&plan.delete_dirs) {
            rows.push((RowKind::Delete, format!("{}/", rel.display())));
        }
        for warning in &plan.warnings {
            rows.push((RowKind::Warning, warning.clone()));
        }
    }
    rows
}

fn scan_all(
    specs: &[PairSpec],
    options: &ScanOptions,
    ctx: &RunContext,
    tx: &Sender<Message>,
) -> Option<Vec<Plan>> {
    let mut plans = Vec::with_capacity(specs.len());
    for spec in specs {
        ctx.progress.reset();
        let started = Instant::now();
        match plan::scan(spec, options, ctx) {
            Ok(plan) => {
                let took = started.elapsed().as_secs_f64();
                let label = pair_label(&plan);
                let copies = plan.copies.len();
                let deletes = plan.delete_count();
                let how = match &plan.dest_scan {
                    DestScan::Cached => tr("cache", "キャッシュ").to_owned(),
                    DestScan::Full(reason) => reason.clone(),
                };
                match &plan.source_scan {
                    SourceScan::Disk => {}
                    SourceScan::Journal { changed_folders } => {
                        let _ = tx.send(Message::Log(trf!(
                            "Source read through the change journal: {changed_folders} folders changed",
                            "変更ジャーナルで元フォルダを確認: 変更 {changed_folders} フォルダ"
                        )));
                    }
                    SourceScan::JournalUnused { reason, .. } => {
                        let _ = tx.send(Message::Log(trf!(
                            "Source listed from disk: {reason}",
                            "元フォルダを通常どおり読み取り: {reason}"
                        )));
                    }
                }
                let _ = tx.send(Message::Log(trf!(
                    "Compared in {took:.2} s: {label}: copy {copies}, delete {deletes} ({how})",
                    "比較 {took:.2}秒: {label}: コピー {copies}、削除 {deletes}({how})"
                )));
                plans.push(plan);
            }
            Err(e) => {
                let _ = tx.send(Message::Log(trf!("Stopped: {e}", "中止: {e}")));
                return None;
            }
        }
    }
    Some(plans)
}

fn execute_all(plans: Vec<Plan>, allow: bool, ctx: &RunContext, tx: &Sender<Message>) {
    for plan in plans {
        let label = pair_label(&plan);
        let started = Instant::now();
        match exec::execute(plan, ctx, allow) {
            Ok(report) => {
                let took = started.elapsed().as_secs_f64();
                let size = format_bytes(report.copied_bytes);
                let errors = report.errors + report.scan_errors;
                let _ = tx.send(Message::Log(trf!(
                    "Done in {took:.2} s: {label}: copied {} ({size}), deleted {}, errors {errors}",
                    "完了 {took:.2}秒: {label}: コピー {} ({size})、削除 {}、エラー {errors}",
                    report.copied,
                    report.deleted,
                )));
            }
            Err(e) => {
                let _ = tx.send(Message::Log(trf!(
                    "Stopped: {label}: {e}",
                    "中止: {label}: {e}"
                )));
                if matches!(e, MirrorError::Cancelled | MirrorError::Guard(_)) {
                    return;
                }
            }
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>) -> Self {
        load_fonts(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let config_path = config::config_path();
        let (mut config, mut load_error) = match config::load(&config_path) {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        };
        i18n::set_lang(Lang::from_code(&config.language));
        let mut auto_start = None;
        if let Some(path) = &file {
            match config::load_pattern_file(path) {
                Ok(pattern) => {
                    auto_start = Some(pattern.run_on_open);
                    config.patterns = vec![pattern];
                    load_error = None;
                }
                Err(e) => {
                    config.patterns.clear();
                    load_error = Some(e);
                }
            }
        }
        let mut app = Self {
            tab: Tab::Run,
            config,
            config_path,
            load_error,
            selected: 0,
            excludes_text: String::new(),
            dirty_since: None,
            full_check: false,
            allow_guarded: false,
            plans: None,
            rows: Vec::new(),
            log: Vec::new(),
            job: None,
            file_mode: file,
            auto_start,
            journal_remote: None,
            journal_declined: false,
        };
        app.sync_excludes_text();
        // A pattern file asks at once, so the prompt comes when it is opened
        // and not partway through its preview or run.
        if app.file_mode.is_some() {
            app.ensure_journal_access();
        }
        app
    }

    /// Asks for administrator rights if the selected pattern uses the change
    /// journal on a drive that needs them. Declining means a normal scan.
    fn ensure_journal_access(&mut self) {
        if self.journal_remote.is_some() || self.journal_declined {
            return;
        }
        let Some(pattern) = self.config.patterns.get(self.selected) else {
            return;
        };
        let needed = pattern.use_change_journal
            && pattern
                .pairs
                .iter()
                .any(|pair| journal::needs_elevation(Path::new(pair.source.trim())));
        if !needed {
            return;
        }
        match elevate::start() {
            Ok(remote) => self.journal_remote = Some(remote),
            Err(e) => {
                self.journal_declined = true;
                self.log.push(trf!(
                    "The change journal will not be used in this window: {e}",
                    "このウィンドウでは変更ジャーナルを使いません: {e}"
                ));
            }
        }
    }

    fn sync_excludes_text(&mut self) {
        self.excludes_text = self
            .config
            .patterns
            .get(self.selected)
            .map_or(String::new(), |p| p.excludes.join("\n"));
    }

    fn touch(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn save_now(&mut self) {
        self.dirty_since = None;
        if self.load_error.is_some() {
            return;
        }
        let saved = match &self.file_mode {
            Some(path) => match self.config.patterns.first() {
                Some(pattern) => config::save_pattern_file(path, pattern),
                None => Ok(()),
            },
            None => config::save(&self.config_path, &self.config),
        };
        if let Err(e) = saved {
            self.log.push(e);
        }
    }

    fn clear_preview(&mut self) {
        self.plans = None;
        self.rows.clear();
        self.allow_guarded = false;
    }

    fn specs(&mut self) -> Option<(Vec<PairSpec>, ScanOptions)> {
        self.ensure_journal_access();
        let pattern = self.config.patterns.get(self.selected)?;
        let mut specs = Vec::new();
        for pair in &pattern.pairs {
            if pair.source.trim().is_empty() || pair.dest.trim().is_empty() {
                self.log.push(
                    tr(
                        "Choose both a source and a destination.",
                        "元と保存先の両方を指定してください",
                    )
                    .to_owned(),
                );
                return None;
            }
            specs.push(PairSpec {
                source: PathBuf::from(pair.source.trim()),
                dest: PathBuf::from(pair.dest.trim()),
                excludes: pattern
                    .excludes
                    .iter()
                    .chain(&pair.excludes)
                    .cloned()
                    .collect(),
            });
        }
        if specs.is_empty() {
            self.log
                .push(tr("Add a folder pair first.", "フォルダの組を追加してください").to_owned());
            return None;
        }
        let options = ScanOptions {
            force_full: self.full_check,
            full_check_days: pattern.full_check_days,
            use_journal: pattern.use_change_journal,
            copy_links: pattern.copy_links,
            journal_remote: self.journal_remote.clone(),
            scan_threads: None,
            copy_threads: None,
        };
        Some((specs, options))
    }

    fn spawn(
        &mut self,
        ui_ctx: &egui::Context,
        work: impl FnOnce(RunContext, Sender<Message>) + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let log_tx = tx.clone();
        let log_ui = ui_ctx.clone();
        let ctx = RunContext::new(Arc::new(move |line| {
            let _ = log_tx.send(Message::Log(line));
            log_ui.request_repaint();
        }));
        let thread_ctx = ctx.clone();
        let done_ui = ui_ctx.clone();
        std::thread::spawn(move || {
            work(thread_ctx, tx.clone());
            let _ = tx.send(Message::Finished);
            done_ui.request_repaint();
        });
        self.job = Some((ctx, rx));
    }

    fn start_preview(&mut self, ui_ctx: &egui::Context) {
        let Some((specs, options)) = self.specs() else {
            return;
        };
        self.save_now();
        self.clear_preview();
        self.spawn(ui_ctx, move |ctx, tx| {
            if let Some(plans) = scan_all(&specs, &options, &ctx, &tx) {
                let _ = tx.send(Message::Plans(plans));
            }
        });
    }

    fn start_run(&mut self, ui_ctx: &egui::Context) {
        let Some((specs, options)) = self.specs() else {
            return;
        };
        self.save_now();
        self.clear_preview();
        self.spawn(ui_ctx, move |ctx, tx| {
            let Some(plans) = scan_all(&specs, &options, &ctx, &tx) else {
                return;
            };
            if plans.iter().any(|p| p.guard.is_some()) {
                let _ = tx.send(Message::Log(
                    tr(
                        "A safety check stopped the run. Nothing was changed.",
                        "安全装置が働いたため、何も変更していません",
                    )
                    .to_owned(),
                ));
                let _ = tx.send(Message::Plans(plans));
                return;
            }
            execute_all(plans, false, &ctx, &tx);
        });
    }

    fn start_execute(&mut self, ui_ctx: &egui::Context) {
        let Some(plans) = self.plans.take() else {
            return;
        };
        let allow = self.allow_guarded;
        self.clear_preview();
        self.spawn(ui_ctx, move |ctx, tx| execute_all(plans, allow, &ctx, &tx));
    }

    fn poll_job(&mut self) {
        let Some((_, rx)) = &self.job else {
            return;
        };
        let mut finished = false;
        let mut logs = Vec::new();
        let mut received = None;
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Log(line) => logs.push(line),
                Message::Plans(plans) => received = Some(plans),
                Message::Finished => finished = true,
            }
        }
        self.log.extend(logs);
        if let Some(plans) = received {
            self.rows = build_rows(&plans);
            self.plans = Some(plans);
        }
        if finished {
            self.job = None;
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(
            RichText::new("Hayakagami")
                .size(18.0)
                .strong()
                .color(ACCENT),
        );
        ui.add_space(10.0);
        let busy = self.job.is_some();
        if let Some(path) = &self.file_mode {
            section_title(ui, tr("PATTERN FILE", "パターンファイル"));
            ui.label(short_name(path))
                .on_hover_text(path.display().to_string());
        } else {
            section_title(ui, tr("PATTERNS", "パターン"));
            let mut selection = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height((ui.available_height() - 130.0).max(40.0))
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        for (i, pattern) in self.config.patterns.iter().enumerate() {
                            if ui
                                .selectable_label(i == self.selected, pattern.name.as_str())
                                .clicked()
                            {
                                selection = Some(i);
                            }
                        }
                    });
                });
            if let Some(i) = selection {
                if !busy && i != self.selected {
                    self.selected = i;
                    self.sync_excludes_text();
                    self.clear_preview();
                }
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            self.settings_menu(ui);
            if self.file_mode.is_some() {
                return;
            }
            ui.separator();
            ui.add_enabled_ui(!busy, |ui| {
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 28.0],
                        egui::Button::new(tr("Open file", "ファイルを開く")),
                    )
                    .clicked()
                {
                    self.import_pattern();
                }
                if ui
                    .add_sized(
                        [width, 28.0],
                        egui::Button::new(tr("New pattern", "新規パターン")),
                    )
                    .clicked()
                {
                    let name = trf!("Pattern {}", "パターン{}", self.config.patterns.len() + 1);
                    self.config.patterns.push(Pattern::new(name));
                    self.tab = Tab::Settings;
                    self.selected = self.config.patterns.len() - 1;
                    self.sync_excludes_text();
                    self.clear_preview();
                    self.touch();
                }
            });
        });
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button(tr("Preferences", "環境設定"), |ui| {
            section_title(ui, tr("LANGUAGE", "言語"));
            let current = i18n::lang();
            let mut chosen = current;
            ui.radio_value(&mut chosen, Lang::En, "English");
            ui.radio_value(&mut chosen, Lang::Ja, "日本語");
            if chosen != current {
                i18n::set_lang(chosen);
                self.config.language = chosen.code().to_owned();
                if self.file_mode.is_some() {
                    if let Err(e) = config::save_language(&self.config_path, chosen.code()) {
                        self.log.push(e);
                    }
                } else {
                    self.touch();
                }
            }
            if cfg!(windows) {
                ui.separator();
                section_title(ui, tr("PATTERN FILES", "パターンファイル"));
                self.file_type_buttons(ui);
            }
        });
    }

    fn file_type_buttons(&mut self, ui: &mut egui::Ui) {
        let mut message = None;
        if ui
            .button(tr("Link .hkgm files", ".hkgm を関連付け"))
            .on_hover_text(tr(
                "Double-clicking a .hkgm file then opens it here. Applies to your Windows user only.",
                ".hkgm をダブルクリックすると、ここで開きます。今のWindowsユーザーだけに適用します",
            ))
            .clicked()
        {
            let result = std::env::current_exe().and_then(|exe| platform::register_file_type(&exe));
            message = Some(match result {
                Ok(()) => tr(
                    ".hkgm files now open with this app.",
                    ".hkgm をこのアプリで開くように設定しました",
                )
                .to_owned(),
                Err(e) => e.to_string(),
            });
        }
        if ui
            .button(tr("Unlink .hkgm files", ".hkgm の関連付け解除"))
            .clicked()
        {
            message = Some(match platform::unregister_file_type() {
                Ok(()) => tr(".hkgm link removed.", ".hkgm の関連付けを解除しました").to_owned(),
                Err(e) => e.to_string(),
            });
        }
        if let Some(message) = message {
            self.log.push(message);
        }
    }

    fn import_pattern(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter(
                tr("Hayakagami pattern", "Hayakagami パターン"),
                &[config::PATTERN_EXTENSION],
            )
            .pick_file();
        let Some(path) = picked else {
            return;
        };
        match config::load_pattern_file(&path) {
            Ok(pattern) => {
                self.config.patterns.push(pattern);
                self.selected = self.config.patterns.len() - 1;
                self.sync_excludes_text();
                self.clear_preview();
                self.touch();
            }
            Err(e) => self.log.push(e),
        }
    }

    fn export_pattern(&mut self) {
        let Some(pattern) = self.config.patterns.get(self.selected) else {
            return;
        };
        let picked = rfd::FileDialog::new()
            .set_file_name(format!("{}.{}", pattern.name, config::PATTERN_EXTENSION))
            .add_filter(
                tr("Hayakagami pattern", "Hayakagami パターン"),
                &[config::PATTERN_EXTENSION],
            )
            .save_file();
        let Some(path) = picked else {
            return;
        };
        let message = match config::save_pattern_file(&path, pattern) {
            Ok(()) => trf!("Saved {}", "{} に保存しました", path.display()),
            Err(e) => e,
        };
        self.log.push(message);
    }

    fn confirm_remove_pattern(&mut self) {
        let name = self.config.patterns[self.selected].name.clone();
        let answer = rfd::MessageDialog::new()
            .set_title(tr("Delete pattern", "パターンの削除"))
            .set_description(trf!(
                "Delete the settings of \"{name}\"? Files in the folders are not touched.",
                "パターン「{name}」の設定を削除しますか?フォルダ内のファイルは削除されません。"
            ))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        if answer == rfd::MessageDialogResult::Yes {
            self.config.patterns.remove(self.selected);
            self.selected = self
                .selected
                .min(self.config.patterns.len().saturating_sub(1));
            self.sync_excludes_text();
            self.clear_preview();
            self.touch();
        }
    }

    fn editor(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut save_file = false;
        let mut delete = false;
        let can_delete = self.file_mode.is_none();
        let Some(pattern) = self.config.patterns.get_mut(self.selected) else {
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(tr(
                    "Press New pattern to get started.",
                    "「新規パターン」を押して始めてください",
                ));
            });
            return;
        };
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let buttons = if can_delete { 230.0 } else { 120.0 };
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut pattern.name)
                            .desired_width((ui.available_width() - buttons).max(120.0))
                            .font(egui::TextStyle::Heading),
                    )
                    .changed();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if can_delete && ui.button(tr("Delete", "削除")).clicked() {
                        delete = true;
                    }
                    if ui.button(tr("Save as file", "ファイルに保存")).clicked() {
                        save_file = true;
                    }
                });
            });
            ui.add_space(6.0);
            changed |= folder_table(ui, &mut pattern.pairs);
            ui.add_space(6.0);
            section_title(ui, tr("EXCLUSIONS", "除外"));
            let response = ui.add(
                egui::TextEdit::multiline(&mut self.excludes_text)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr(
                        "None. To exclude items, add one per line, such as *.tmp or build/",
                        "なし。除外するときは1行に1つ書きます(例: *.tmp、build/)",
                    )),
            );
            if response.changed() {
                pattern.excludes = self.excludes_text.lines().map(str::to_owned).collect();
                changed = true;
            }
            egui::CollapsingHeader::new(tr("Advanced", "詳細設定"))
                .id_salt("advanced")
                .show(ui, |ui| {
                    changed |= ui
                        .checkbox(
                            &mut pattern.run_on_open,
                            tr(
                                "Run right away when this pattern's file is opened (otherwise preview)",
                                "パターンファイルを開いたらすぐ実行する(オフならプレビュー)",
                            ),
                        )
                        .on_hover_text(tr(
                            "Safety checks still stop the run.",
                            "この場合も安全装置は働きます",
                        ))
                        .changed();
                    changed |= ui
                        .checkbox(
                            &mut pattern.copy_links,
                            tr(
                                "Copy links as links, with their targets unchanged",
                                "リンクをリンクのままコピーする(リンク先は変更しない)",
                            ),
                        )
                        .on_hover_text(tr(
                            "What a link points to is not copied, and an absolute link in the destination still points to the original place. On Windows, symbolic links need Developer Mode or administrator rights; without them they are skipped.",
                            "リンク先の中身はコピーしません。絶対パスのリンクは、保存先でも元の場所を指します。Windowsのシンボリックリンクには開発者モードか管理者権限が必要で、ない場合はスキップします",
                        ))
                        .changed();
                    changed |= ui
                        .checkbox(
                            &mut pattern.use_change_journal,
                            tr(
                                "Use the NTFS change journal to skip unchanged folders",
                                "NTFSの変更ジャーナルで、変更のないフォルダを読み飛ばす",
                            ),
                        )
                        .on_hover_text(tr(
                            "Windows asks for permission when a drive needs administrator rights. The rights are used only to read the journal. Declining means a normal scan.",
                            "管理者権限が必要なドライブでは、Windowsが許可を求めます。権限は変更ジャーナルの読み取りだけに使います。許可しない場合は通常どおりスキャンします",
                        ))
                        .changed();
                    ui.horizontal(|ui| {
                        ui.label(tr("Full check every", "フルチェックの間隔"));
                        changed |= ui
                            .add(egui::DragValue::new(&mut pattern.full_check_days).range(0..=3650))
                            .changed();
                        ui.label(tr(
                            "days (0 turns the cache off)",
                            "日(0でキャッシュを使わない)",
                        ));
                    });
                    for (i, pair) in pattern.pairs.iter_mut().enumerate() {
                        ui.push_id(i, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(trf!(
                                    "Pair {} exclusions",
                                    "{} 組目だけの除外",
                                    i + 1
                                ));
                                let mut text = pair.excludes.join("; ");
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut text)
                                            .desired_width(f32::INFINITY)
                                            .hint_text(tr("Separate with ;", "; で区切ります")),
                                    )
                                    .changed()
                                {
                                    pair.excludes = text
                                        .split(';')
                                        .map(|s| s.trim_start().to_owned())
                                        .collect();
                                    if pair.excludes.iter().all(String::is_empty) {
                                        pair.excludes.clear();
                                    }
                                    changed = true;
                                }
                            });
                        });
                    }
                });
        });
        if changed {
            self.touch();
            self.clear_preview();
        }
        if save_file {
            self.export_pattern();
        }
        if delete {
            self.confirm_remove_pattern();
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Run, tr("Run", "実行")),
                (Tab::Settings, tr("Pattern settings", "パターン設定")),
            ] {
                let selected = self.tab == tab;
                let text = RichText::new(label).size(15.0).strong();
                let text = if selected {
                    text.color(ACCENT)
                } else {
                    text.color(MUTED)
                };
                let response = ui.add(egui::Button::new(text).frame(false));
                if selected {
                    let rect = response.rect;
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom() + 1.0,
                        Stroke::new(2.0, ACCENT),
                    );
                }
                if response.clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.separator();
    }

    /// What the Run tab will act on, read-only.
    fn run_summary(&self, ui: &mut egui::Ui) {
        let Some(pattern) = self.config.patterns.get(self.selected) else {
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(tr(
                    "Press New pattern to get started.",
                    "「新規パターン」を押して始めてください",
                ));
            });
            return;
        };
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(pattern.name.as_str()).heading());
            let empty = |text: &str| text.trim().is_empty();
            if pattern
                .pairs
                .iter()
                .all(|p| empty(&p.source) && empty(&p.dest))
            {
                ui.label(
                    RichText::new(tr(
                        "No folders yet. Choose them in Pattern settings.",
                        "フォルダが未設定です。「パターン設定」で選んでください",
                    ))
                    .color(MUTED),
                );
            }
            for pair in &pattern.pairs {
                if empty(&pair.source) && empty(&pair.dest) {
                    continue;
                }
                let line = format!("{}  →  {}", pair.source, pair.dest);
                ui.add(egui::Label::new(RichText::new(line).color(MUTED)).truncate());
            }
            let excludes = pattern
                .excludes
                .iter()
                .filter(|e| !e.trim().is_empty())
                .count();
            if excludes > 0 {
                ui.label(
                    RichText::new(trf!("{excludes} exclusions", "除外 {excludes} 件"))
                        .color(MUTED)
                        .size(12.0),
                );
            }
        });
    }

    fn results(&mut self, ui: &mut egui::Ui) {
        let Some(plans) = &self.plans else {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(tr(
                        "Preview shows what will be copied and deleted, before anything changes.",
                        "プレビューでは、変更する前にコピーと削除の内容を確認できます",
                    ))
                    .color(MUTED),
                );
            });
            return;
        };
        let copies: usize = plans.iter().map(|p| p.copies.len()).sum();
        let bytes: u64 = plans.iter().map(|p| p.stats.copy_bytes).sum();
        let deletes: usize = plans.iter().map(Plan::delete_count).sum();
        let unchanged: u64 = plans.iter().map(|p| p.stats.unchanged).sum();
        let cached = plans.iter().all(|p| p.dest_scan == DestScan::Cached);
        let reason = plans.iter().find_map(|p| match &p.dest_scan {
            DestScan::Full(reason) => Some(reason.clone()),
            DestScan::Cached => None,
        });
        let guards: Vec<String> = plans.iter().filter_map(|p| p.guard.clone()).collect();

        ui.columns(4, |columns| {
            tile(
                &mut columns[0],
                tr("Copy", "コピー"),
                copies.to_string(),
                Some(format_bytes(bytes)),
                GREEN,
            );
            tile(
                &mut columns[1],
                tr("Delete", "削除"),
                deletes.to_string(),
                None,
                if deletes > 0 { RED } else { TEXT },
            );
            tile(
                &mut columns[2],
                tr("Unchanged", "変更なし"),
                unchanged.to_string(),
                None,
                TEXT,
            );
            let how = if cached {
                tr("Cache", "キャッシュ")
            } else {
                tr("Full", "全一覧")
            };
            tile(
                &mut columns[3],
                tr("Destination read", "保存先の読み方"),
                how.to_owned(),
                reason,
                ACCENT,
            );
        });

        if !guards.is_empty() {
            Frame::new()
                .fill(RED_SOFT)
                .stroke(Stroke::new(1.0, RED))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for guard in &guards {
                        ui.label(RichText::new(guard).color(RED));
                    }
                    ui.checkbox(
                        &mut self.allow_guarded,
                        tr(
                            "I checked this and allow these changes",
                            "内容を確認したので、この変更を許可する",
                        ),
                    );
                });
        }

        if self.rows.is_empty() {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(tr("Nothing to do.", "変更はありません")).color(MUTED));
            });
            return;
        }
        let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        card().inner_margin(8).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::both()
                .id_salt("rows")
                .auto_shrink([false, false])
                .show_rows(ui, height, self.rows.len(), |ui, range| {
                    for (kind, text) in &self.rows[range] {
                        ui.horizontal(|ui| {
                            ui.set_height(height);
                            fixed_label(
                                ui,
                                64.0,
                                RichText::new(kind.label()).color(kind.color()).strong(),
                            );
                            ui.add(egui::Label::new(text.as_str()).extend());
                        });
                    }
                });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, ui_ctx: &egui::Context) {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(trf!("Log ({})", "ログ ({})", self.log.len()))
            .id_salt("log")
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("log-lines")
                    .max_height(130.0)
                    .stick_to_bottom(true)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.add(egui::Label::new(line.as_str()).wrap());
                        }
                    });
            });
        let busy = self.job.is_some();
        let has_pattern = self.selected < self.config.patterns.len();
        let has_plan = self.plans.is_some();
        let blocked = self
            .plans
            .as_ref()
            .is_some_and(|plans| plans.iter().any(|p| p.guard.is_some()) && !self.allow_guarded);
        let nothing = self
            .plans
            .as_ref()
            .is_some_and(|plans| !plans.iter().any(Plan::has_work));
        let on_run_tab = self.tab == Tab::Run;
        let bar = egui::vec2(ui.available_width(), 34.0);
        ui.allocate_ui_with_layout(
            bar,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if !on_run_tab {
                    if ui
                        .add_enabled(has_pattern, primary(tr("Done", "完了")))
                        .clicked()
                    {
                        self.tab = Tab::Run;
                    }
                } else if busy {
                    if ui.button(tr("Stop", "中止")).clicked() {
                        if let Some((ctx, _)) = &self.job {
                            ctx.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    ui.spinner();
                } else {
                    let label = if has_plan {
                        tr("Run this plan", "この内容で実行")
                    } else {
                        tr("Run", "実行")
                    };
                    let ready = has_pattern && !blocked && !nothing;
                    if ui.add_enabled(ready, primary(label)).clicked() {
                        if has_plan {
                            self.start_execute(ui_ctx);
                        } else {
                            self.start_run(ui_ctx);
                        }
                    }
                    if ui
                        .add_enabled(has_pattern, egui::Button::new(tr("Preview", "プレビュー")))
                        .clicked()
                    {
                        self.start_preview(ui_ctx);
                    }
                    let full = ui
                        .add_enabled(
                            has_pattern,
                            egui::Checkbox::new(
                                &mut self.full_check,
                                tr("Full check", "フルチェック"),
                            ),
                        )
                        .on_hover_text(tr(
                            "Read the destination from disk instead of the saved state",
                            "記録を使わず、保存先もディスクから読みます",
                        ));
                    if full.changed() {
                        self.clear_preview();
                    }
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    self.status_line(ui);
                });
            },
        );
        ui.add_space(6.0);
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        let Some((ctx, _)) = &self.job else {
            let text = self
                .log
                .last()
                .map_or(tr("Ready", "準備完了"), String::as_str);
            ui.add(egui::Label::new(RichText::new(text).color(MUTED)).truncate());
            return;
        };
        let p = &ctx.progress;
        match p.phase() {
            Phase::Scanning | Phase::Idle => {
                let (source, dest) = (
                    p.source_entries.load(Ordering::Relaxed),
                    p.dest_entries.load(Ordering::Relaxed),
                );
                ui.add(
                    egui::Label::new(trf!(
                        "Comparing: source {source}, destination {dest}",
                        "比較中: 元 {source} 件、保存先 {dest} 件"
                    ))
                    .truncate(),
                );
            }
            Phase::Copying => {
                let done = p.bytes_done.load(Ordering::Relaxed);
                let total = p.bytes_total.load(Ordering::Relaxed).max(1);
                let (files, all) = (
                    p.files_done.load(Ordering::Relaxed),
                    p.files_total.load(Ordering::Relaxed),
                );
                let (done_text, total_text) = (format_bytes(done), format_bytes(total));
                ui.add(
                    egui::ProgressBar::new(done as f32 / total as f32).text(trf!(
                        "Copying {files} / {all} files, {done_text} / {total_text}",
                        "コピー中 {files} / {all} ファイル、{done_text} / {total_text}"
                    )),
                );
            }
            Phase::Deleting => {
                let (done, all) = (
                    p.deletes_done.load(Ordering::Relaxed),
                    p.deletes_total.load(Ordering::Relaxed),
                );
                ui.label(trf!("Deleting {done} / {all}", "削除中 {done} / {all}"));
            }
        }
    }
}

fn browse(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if Path::new(current).is_dir() {
        dialog = dialog.set_directory(current);
    }
    dialog.pick_folder().map(|p| p.display().to_string())
}

fn fixed_label(ui: &mut egui::Ui, width: f32, text: impl Into<egui::WidgetText>) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.spacing().interact_size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(egui::Label::new(text).truncate());
        },
    );
}

/// Source and destination side by side, one row per folder pair.
fn folder_table(ui: &mut egui::Ui, pairs: &mut Vec<FolderPair>) -> bool {
    const BROWSE: f32 = 72.0;
    const ARROW: f32 = 18.0;
    const REMOVE: f32 = 28.0;
    let gap = ui.spacing().item_spacing.x;
    let field =
        ((ui.available_width() - BROWSE * 2.0 - ARROW - REMOVE - gap * 5.0) / 2.0).max(90.0);
    let mut changed = false;
    let mut remove = None;
    ui.horizontal(|ui| {
        fixed_label(
            ui,
            field + BROWSE + gap + ARROW + gap,
            RichText::new(tr("SOURCE", "元"))
                .color(MUTED)
                .size(12.5)
                .strong(),
        );
        fixed_label(
            ui,
            field,
            RichText::new(tr("DESTINATION", "保存先"))
                .color(MUTED)
                .size(12.5)
                .strong(),
        );
    });
    let height = ui.spacing().interact_size.y;
    let removable = pairs.len() > 1;
    for (i, pair) in pairs.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            let row = egui::vec2(ui.available_width(), height + 6.0);
            let layout = egui::Layout::left_to_right(egui::Align::Center);
            ui.allocate_ui_with_layout(row, layout, |ui| {
                changed |= path_field(
                    ui,
                    &mut pair.source,
                    tr("Folder to back up", "バックアップするフォルダ"),
                    field,
                    BROWSE,
                );
                ui.add_sized(
                    [ARROW, height],
                    egui::Label::new(RichText::new("→").color(MUTED)),
                );
                changed |= path_field(
                    ui,
                    &mut pair.dest,
                    tr("Mirror folder", "ミラー先のフォルダ"),
                    field,
                    BROWSE,
                );
                if removable
                    && ui
                        .add_sized([REMOVE, height], egui::Button::new("×"))
                        .on_hover_text(tr("Remove this pair", "この組を外す"))
                        .clicked()
                {
                    remove = Some(i);
                }
            });
        });
    }
    if let Some(i) = remove {
        pairs.remove(i);
        changed = true;
    }
    if ui
        .button(tr("Add folder pair", "フォルダの組を追加"))
        .clicked()
    {
        pairs.push(FolderPair::default());
        changed = true;
    }
    changed
}

fn path_field(
    ui: &mut egui::Ui,
    value: &mut String,
    hint: &str,
    width: f32,
    browse_width: f32,
) -> bool {
    let height = ui.spacing().interact_size.y;
    let mut changed = ui
        .add_sized(
            [width, height],
            egui::TextEdit::singleline(value).hint_text(hint),
        )
        .changed();
    if ui
        .add_sized(
            [browse_width, height],
            egui::Button::new(tr("Browse", "参照")),
        )
        .clicked()
    {
        if let Some(path) = browse(value) {
            *value = path;
            changed = true;
        }
    }
    changed
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ui_ctx = ui.ctx().clone();
        self.poll_job();
        if let Some(run) = self.auto_start.take() {
            if run {
                self.start_run(&ui_ctx);
            } else {
                self.start_preview(&ui_ctx);
            }
        }
        if self.job.is_some() {
            ui_ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(since) = self.dirty_since {
            if since.elapsed() > Duration::from_secs(1) {
                self.save_now();
            } else {
                ui_ctx.request_repaint_after(Duration::from_millis(1100));
            }
        }

        egui::Panel::left("patterns")
            .default_size(180.0)
            .resizable(false)
            .show(ui, |ui| self.sidebar(ui));
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, &ui_ctx));
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.load_error {
                ui.colored_label(
                    RED,
                    trf!(
                        "Settings could not be read, so saving is off: {error}",
                        "設定を読み込めないため、保存を止めています: {error}"
                    ),
                );
            }
            self.tab_bar(ui);
            match self.tab {
                Tab::Run => {
                    self.run_summary(ui);
                    self.results(ui);
                }
                Tab::Settings => {
                    let busy = self.job.is_some();
                    egui::ScrollArea::vertical()
                        .id_salt("editor")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width() - 16.0);
                            ui.add_enabled_ui(!busy, |ui| self.editor(ui));
                        });
                }
            }
        });
    }

    fn on_exit(&mut self) {
        if self.dirty_since.is_some() {
            self.save_now();
        }
    }
}

fn main() -> eframe::Result {
    let mut args = std::env::args_os().skip(1);
    let file = args.next();
    if file.as_deref() == Some(std::ffi::OsStr::new(elevate::HELPER_ARG)) {
        let pipe = args.next().unwrap_or_default();
        std::process::exit(elevate::helper_main(&pipe.to_string_lossy()));
    }
    let file = file.map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Hayakagami")
            .with_inner_size([980.0, 720.0])
            .with_min_inner_size([720.0, 520.0])
            .with_icon(Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png"))
                    .expect("the bundled icon is a valid PNG"),
            )),
        ..Default::default()
    };
    eframe::run_native(
        "Hayakagami",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, file)))),
    )
}
