//! Command line interface.

use clap::{Parser, Subcommand};
use hayakagami::config::{self, FolderPair};
use hayakagami::exec::{self, Report};
use hayakagami::model::{MirrorError, PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "hayakagami", version, about = "フォルダ単位のミラーバックアップ")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Clone)]
struct RunFlags {
    /// 比較だけ行い、何も変更しない
    #[arg(long)]
    dry_run: bool,
    /// キャッシュを使わず保存先も一覧する
    #[arg(long)]
    full: bool,
    /// 安全装置が止めた削除も実行する
    #[arg(long)]
    allow_delete: bool,
    /// 走査スレッド数(ディスクごとの自動設定を上書き)
    #[arg(long)]
    scan_threads: Option<usize>,
    /// コピースレッド数(自動設定を上書き)
    #[arg(long)]
    copy_threads: Option<usize>,
    /// 変更内容を1件ずつ表示する
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// 保存済みパターンを実行する
    Run {
        pattern: String,
        #[command(flatten)]
        flags: RunFlags,
    },
    /// フォルダを直接指定してミラーする
    Mirror {
        source: PathBuf,
        dest: PathBuf,
        /// 除外パターン(複数指定可)
        #[arg(short = 'x', long = "exclude")]
        excludes: Vec<String>,
        #[command(flatten)]
        flags: RunFlags,
    },
    /// 保存済みパターンを一覧する
    List,
}

fn print_plan(plan: &Plan, verbose: bool, took: f64) {
    let how = match &plan.dest_scan {
        DestScan::Cached => "キャッシュ".to_string(),
        DestScan::Full(reason) => format!("全一覧({reason})"),
    };
    println!("  比較: {took:.2}秒 / 保存先の読み方: {how}");
    println!(
        "  元 {} ファイル, {} フォルダ / 変更なし {} / コピー {} ({:.1} MiB) / 削除 {}",
        plan.stats.source_files,
        plan.stats.source_dirs,
        plan.stats.unchanged,
        plan.copies.len(),
        plan.stats.copy_bytes as f64 / 1_048_576.0,
        plan.delete_count(),
    );
    for warning in &plan.warnings {
        println!("  警告: {warning}");
    }
    if verbose {
        for item in &plan.copies {
            let mark = if item.is_new { "新規" } else { "更新" };
            println!("  {mark} {}", item.rel.display());
        }
        for rel in plan.conflict_files.iter().chain(&plan.delete_files) {
            println!("  削除 {}", rel.display());
        }
        for rel in plan.conflict_dirs.iter().chain(&plan.delete_dirs) {
            println!("  削除 {}/", rel.display());
        }
    }
}

/// Returns true when everything succeeded.
fn run_pair(spec: PairSpec, flags: &RunFlags, days: u32, ctx: &RunContext) -> Result<bool, MirrorError> {
    println!("{} -> {}", spec.source.display(), spec.dest.display());
    let options = ScanOptions {
        force_full: flags.full,
        full_check_days: days,
        scan_threads: flags.scan_threads,
        copy_threads: flags.copy_threads,
    };
    ctx.progress.reset();
    let started = Instant::now();
    let plan = plan::scan(&spec, &options, ctx)?;
    print_plan(&plan, flags.verbose, started.elapsed().as_secs_f64());
    if let Some(reason) = &plan.guard {
        println!("  安全装置: {reason}");
        if !flags.dry_run && !flags.allow_delete {
            return Err(MirrorError::Guard(
                "何も変更していません。内容を確認して、問題なければ --allow-delete を付けてください"
                    .into(),
            ));
        }
    }
    if flags.dry_run {
        return Ok(plan.scan_errors == 0);
    }
    let started = Instant::now();
    let report: Report = exec::execute(plan, ctx, flags.allow_delete)?;
    println!(
        "  完了: {:.2}秒 / コピー {} ({:.1} MiB) / 削除 {} / エラー {}",
        started.elapsed().as_secs_f64(),
        report.copied,
        report.copied_bytes as f64 / 1_048_576.0,
        report.deleted,
        report.errors + report.scan_errors,
    );
    Ok(report.errors == 0 && report.scan_errors == 0)
}

fn specs_from(pairs: &[FolderPair], common: &[String]) -> Vec<PairSpec> {
    pairs
        .iter()
        .map(|p| PairSpec {
            source: PathBuf::from(&p.source),
            dest: PathBuf::from(&p.dest),
            excludes: common.iter().chain(&p.excludes).cloned().collect(),
        })
        .collect()
}

fn run_all(specs: Vec<PairSpec>, flags: &RunFlags, days: u32) -> ExitCode {
    let ctx = RunContext::new(Arc::new(|line| eprintln!("  {line}")));
    let mut clean = true;
    for spec in specs {
        match run_pair(spec, flags, days, &ctx) {
            Ok(ok) => clean &= ok,
            Err(e) => {
                eprintln!("  中止: {e}");
                return ExitCode::from(1);
            }
        }
    }
    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Mirror {
            source,
            dest,
            excludes,
            flags,
        } => run_all(
            vec![PairSpec {
                source,
                dest,
                excludes,
            }],
            &flags,
            30,
            ),
        Command::Run { pattern, flags } => {
            let config = match config::load(&config::config_path()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(1);
                }
            };
            let Some(found) = config.patterns.iter().find(|p| p.name == pattern) else {
                eprintln!("パターンが見つかりません: {pattern}");
                return ExitCode::from(1);
            };
            run_all(
                specs_from(&found.pairs, &found.excludes),
                &flags,
                found.full_check_days,
            )
        }
        Command::List => match config::load(&config::config_path()) {
            Ok(config) => {
                for pattern in &config.patterns {
                    println!("{} ({} 組)", pattern.name, pattern.pairs.len());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        },
    }
}
