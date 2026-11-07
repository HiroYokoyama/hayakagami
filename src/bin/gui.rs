#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Desktop interface.

use eframe::egui;
use hayakagami::config::{self, Config, FolderPair, Pattern};
use hayakagami::exec;
use hayakagami::model::{MirrorError, PairSpec, Phase, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

enum Message {
    Log(String),
    Plans(Vec<Plan>),
    Finished,
}

struct App {
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
    rows: Vec<String>,
    log: Vec<String>,
    job: Option<(RunContext, Receiver<Message>)>,
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
        Arc::new(egui::FontData::from_owned(bytes)),
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

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn describe_plan(plan: &Plan) -> String {
    let how = match &plan.dest_scan {
        DestScan::Cached => "キャッシュ".to_string(),
        DestScan::Full(reason) => format!("全一覧: {reason}"),
    };
    format!(
        "{} -> {}: 元 {} ファイル / 変更なし {} / コピー {} ({}) / 削除 {} [{how}]",
        plan.source.display(),
        plan.dest.display(),
        plan.stats.source_files,
        plan.stats.unchanged,
        plan.copies.len(),
        format_bytes(plan.stats.copy_bytes),
        plan.delete_count(),
    )
}

fn build_rows(plans: &[Plan]) -> Vec<String> {
    let mut rows = Vec::new();
    for plan in plans {
        for item in &plan.copies {
            let mark = if item.is_new { "新規" } else { "更新" };
            rows.push(format!("{mark}  {}", item.rel.display()));
        }
        for rel in plan.conflict_files.iter().chain(&plan.delete_files) {
            rows.push(format!("削除  {}", rel.display()));
        }
        for rel in plan.conflict_dirs.iter().chain(&plan.delete_dirs) {
            rows.push(format!("削除  {}/", rel.display()));
        }
        for warning in &plan.warnings {
            rows.push(format!("警告  {warning}"));
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
                let line = format!(
                    "比較 {:.2}秒: {}",
                    started.elapsed().as_secs_f64(),
                    describe_plan(&plan)
                );
                let _ = tx.send(Message::Log(line));
                plans.push(plan);
            }
            Err(e) => {
                let _ = tx.send(Message::Log(format!("中止: {e}")));
                return None;
            }
        }
    }
    Some(plans)
}

fn execute_all(plans: Vec<Plan>, allow: bool, ctx: &RunContext, tx: &Sender<Message>) {
    for plan in plans {
        let label = format!("{} -> {}", plan.source.display(), plan.dest.display());
        let started = Instant::now();
        match exec::execute(plan, ctx, allow) {
            Ok(report) => {
                let _ = tx.send(Message::Log(format!(
                    "完了 {:.2}秒: {label}: コピー {} ({}) / 削除 {} / エラー {}",
                    started.elapsed().as_secs_f64(),
                    report.copied,
                    format_bytes(report.copied_bytes),
                    report.deleted,
                    report.errors + report.scan_errors,
                )));
            }
            Err(e) => {
                let _ = tx.send(Message::Log(format!("中止: {label}: {e}")));
                if matches!(e, MirrorError::Cancelled | MirrorError::Guard(_)) {
                    return;
                }
            }
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        load_fonts(&cc.egui_ctx);
        let config_path = config::config_path();
        let (config, load_error) = match config::load(&config_path) {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let mut app = Self {
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
        };
        app.sync_excludes_text();
        app
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
        if let Err(e) = config::save(&self.config_path, &self.config) {
            self.log.push(e);
        }
    }

    fn clear_preview(&mut self) {
        self.plans = None;
        self.rows.clear();
        self.allow_guarded = false;
    }

    fn specs(&mut self) -> Option<(Vec<PairSpec>, ScanOptions)> {
        let pattern = self.config.patterns.get(self.selected)?;
        let mut specs = Vec::new();
        for pair in &pattern.pairs {
            if pair.source.trim().is_empty() || pair.dest.trim().is_empty() {
                self.log
                    .push("元と保存先の両方を指定してください".to_string());
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
            self.log.push("フォルダの組がありません".to_string());
            return None;
        }
        let options = ScanOptions {
            force_full: self.full_check,
            full_check_days: pattern.full_check_days,
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
                    "安全装置が働いたため、まだ何も変更していません。内容を確認してください".into(),
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

    fn patterns_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("パターン");
        ui.separator();
        let busy = self.job.is_some();
        let mut selection = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                for (i, pattern) in self.config.patterns.iter().enumerate() {
                    if ui
                        .selectable_label(i == self.selected, pattern.name.as_str())
                        .clicked()
                    {
                        selection = Some(i);
                    }
                }
            });
        if let Some(i) = selection {
            if !busy && i != self.selected {
                self.selected = i;
                self.sync_excludes_text();
                self.clear_preview();
            }
        }
        ui.separator();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("追加").clicked() {
                    let name = format!("パターン{}", self.config.patterns.len() + 1);
                    self.config.patterns.push(Pattern::new(name));
                    self.selected = self.config.patterns.len() - 1;
                    self.sync_excludes_text();
                    self.clear_preview();
                    self.touch();
                }
                let can_remove = self.selected < self.config.patterns.len();
                if ui.add_enabled(can_remove, egui::Button::new("削除")).clicked() {
                    let name = self.config.patterns[self.selected].name.clone();
                    let answer = rfd::MessageDialog::new()
                        .set_title("パターンの削除")
                        .set_description(format!(
                            "パターン「{name}」の設定を削除しますか?フォルダ内のファイルは削除されません。"
                        ))
                        .set_buttons(rfd::MessageButtons::YesNo)
                        .show();
                    if answer == rfd::MessageDialogResult::Yes {
                        self.config.patterns.remove(self.selected);
                        self.selected = self.selected.min(self.config.patterns.len().saturating_sub(1));
                        self.sync_excludes_text();
                        self.clear_preview();
                        self.touch();
                    }
                }
            });
        });
    }

    fn pattern_editor(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let selected = self.selected;
        let Some(pattern) = self.config.patterns.get_mut(selected) else {
            ui.label("左の「追加」からパターンを作成してください");
            return;
        };
        ui.horizontal(|ui| {
            ui.label("名前");
            changed |= ui.text_edit_singleline(&mut pattern.name).changed();
            ui.label("定期フルチェック(日)");
            changed |= ui
                .add(egui::DragValue::new(&mut pattern.full_check_days).range(0..=3650))
                .on_hover_text("この日数ごとに保存先も全部読み直します。0でキャッシュを使いません")
                .changed();
        });
        ui.add_space(4.0);
        ui.label("フォルダの組(元 -> 保存先)");
        let mut remove = None;
        egui::ScrollArea::vertical()
            .id_salt("pairs")
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, pair) in pattern.pairs.iter_mut().enumerate() {
                    ui.push_id(i, |ui| {
                        changed |= pair_row(ui, pair, &mut remove, i);
                    });
                }
            });
        if let Some(i) = remove {
            pattern.pairs.remove(i);
            changed = true;
        }
        if ui.button("フォルダの組を追加").clicked() {
            pattern.pairs.push(FolderPair::default());
            changed = true;
        }
        ui.add_space(4.0);
        ui.label("除外パターン(1行に1つ。例: *.tmp / node_modules / build/ / docs/draft)");
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.excludes_text)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        if response.changed() {
            pattern.excludes = self.excludes_text.lines().map(str::to_owned).collect();
            changed = true;
        }
        if changed {
            self.touch();
            self.clear_preview();
        }
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, ui_ctx: &egui::Context) {
        let busy = self.job.is_some();
        let has_pattern = self.selected < self.config.patterns.len();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy && has_pattern, |ui| {
                if ui.button("プレビュー").clicked() {
                    self.start_preview(ui_ctx);
                }
                if ui.button("実行").clicked() {
                    self.start_run(ui_ctx);
                }
                ui.checkbox(&mut self.full_check, "フルチェック(保存先も全部読む)");
            });
            if busy && ui.button("中止").clicked() {
                if let Some((ctx, _)) = &self.job {
                    ctx.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        let Some(plans) = &self.plans else {
            return;
        };
        let guards: Vec<String> = plans.iter().filter_map(|p| p.guard.clone()).collect();
        let summary: Vec<String> = plans.iter().map(describe_plan).collect();
        for line in summary {
            ui.label(line);
        }
        for guard in &guards {
            ui.colored_label(
                egui::Color32::from_rgb(220, 70, 60),
                format!("安全装置: {guard}"),
            );
        }
        ui.horizontal(|ui| {
            if !guards.is_empty() {
                ui.checkbox(
                    &mut self.allow_guarded,
                    "内容を確認したので、この削除を許可する",
                );
            }
            let ready = !busy && (guards.is_empty() || self.allow_guarded);
            if ui
                .add_enabled(ready, egui::Button::new("この内容で実行"))
                .clicked()
            {
                self.start_execute(ui_ctx);
            }
        });
        let height = ui.text_style_height(&egui::TextStyle::Body);
        egui::ScrollArea::both()
            .id_salt("rows")
            .max_height(ui.available_height().max(60.0))
            .auto_shrink([false, false])
            .show_rows(ui, height, self.rows.len(), |ui, range| {
                for row in &self.rows[range] {
                    ui.add(egui::Label::new(row.as_str()).extend());
                }
            });
    }

    fn progress_panel(&self, ui: &mut egui::Ui) {
        if let Some((ctx, _)) = &self.job {
            let p = &ctx.progress;
            match p.phase() {
                Phase::Scanning | Phase::Idle => {
                    ui.label(format!(
                        "比較中: 元 {} 件 / 保存先 {} 件",
                        p.source_entries.load(Ordering::Relaxed),
                        p.dest_entries.load(Ordering::Relaxed)
                    ));
                }
                Phase::Copying => {
                    let done = p.bytes_done.load(Ordering::Relaxed);
                    let total = p.bytes_total.load(Ordering::Relaxed).max(1);
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total as f32).text(format!(
                            "コピー中: {} / {} ファイル, {} / {}",
                            p.files_done.load(Ordering::Relaxed),
                            p.files_total.load(Ordering::Relaxed),
                            format_bytes(done),
                            format_bytes(total)
                        )),
                    );
                }
                Phase::Deleting => {
                    ui.label(format!(
                        "削除中: {} / {} 件",
                        p.deletes_done.load(Ordering::Relaxed),
                        p.deletes_total.load(Ordering::Relaxed)
                    ));
                }
            }
        }
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.log {
                    ui.add(egui::Label::new(line.as_str()).wrap());
                }
            });
    }
}

fn browse(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if Path::new(current).is_dir() {
        dialog = dialog.set_directory(current);
    }
    dialog.pick_folder().map(|p| p.display().to_string())
}

fn pair_row(
    ui: &mut egui::Ui,
    pair: &mut FolderPair,
    remove: &mut Option<usize>,
    i: usize,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = ((ui.available_width() - 190.0) / 2.0).max(80.0);
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut pair.source)
                    .desired_width(width)
                    .hint_text("元フォルダ"),
            )
            .changed();
        if ui.small_button("参照").clicked() {
            if let Some(path) = browse(&pair.source) {
                pair.source = path;
                changed = true;
            }
        }
        ui.label("->");
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut pair.dest)
                    .desired_width(width)
                    .hint_text("保存先フォルダ"),
            )
            .changed();
        if ui.small_button("参照").clicked() {
            if let Some(path) = browse(&pair.dest) {
                pair.dest = path;
                changed = true;
            }
        }
        if ui.small_button("外す").clicked() {
            *remove = Some(i);
        }
    });
    ui.horizontal(|ui| {
        ui.label("  この組だけの除外(;区切り)");
        let mut text = pair.excludes.join("; ");
        if ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(f32::INFINITY))
            .changed()
        {
            pair.excludes = text.split(';').map(|s| s.trim_start().to_owned()).collect();
            if pair.excludes.iter().all(String::is_empty) {
                pair.excludes.clear();
            }
            changed = true;
        }
    });
    changed
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ui_ctx = ui.ctx().clone();
        self.poll_job();
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
            .default_size(170.0)
            .show(ui, |ui| self.patterns_panel(ui));
        egui::Panel::bottom("log")
            .default_size(150.0)
            .resizable(true)
            .show(ui, |ui| self.progress_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.load_error {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 70, 60),
                    format!("設定を読み込めないため、保存を止めています: {error}"),
                );
            }
            let busy = self.job.is_some();
            ui.add_enabled_ui(!busy, |ui| self.pattern_editor(ui));
            ui.separator();
            self.action_bar(ui, &ui_ctx);
        });
    }

    fn on_exit(&mut self) {
        if self.dirty_since.is_some() {
            self.save_now();
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Hayakagami")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Hayakagami",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
