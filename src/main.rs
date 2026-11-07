//! Command line interface.

use clap::{Parser, Subcommand};
use hayakagami::config::{self, FolderPair};
use hayakagami::exec::{self, Report};
use hayakagami::i18n::{self, tr, Lang};
use hayakagami::model::{MirrorError, PairSpec, RunContext, ScanOptions};
use hayakagami::plan::{self, DestScan, Plan};
use hayakagami::trf;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "hayakagami", version, about = "Folder-based mirror backup")]
struct Cli {
    /// Message language: en or ja. Defaults to the saved setting, then English
    #[arg(long, global = true)]
    lang: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Clone)]
struct RunFlags {
    /// Compare only and change nothing
    #[arg(long)]
    dry_run: bool,
    /// Read the destination from disk instead of the saved state
    #[arg(long)]
    full: bool,
    /// Carry out deletions that a safety check stopped
    #[arg(long)]
    allow_delete: bool,
    /// Scan threads, replacing the automatic per-disk setting
    #[arg(long)]
    scan_threads: Option<usize>,
    /// Copy threads, replacing the automatic setting
    #[arg(long)]
    copy_threads: Option<usize>,
    /// List every change
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run a saved pattern, or a pattern file (.hkgm)
    Run {
        pattern: String,
        #[command(flatten)]
        flags: RunFlags,
    },
    /// Mirror two folders given on the command line
    Mirror {
        source: PathBuf,
        dest: PathBuf,
        /// Exclusion pattern (repeatable)
        #[arg(short = 'x', long = "exclude")]
        excludes: Vec<String>,
        #[command(flatten)]
        flags: RunFlags,
    },
    /// List saved patterns
    List,
}

fn print_plan(plan: &Plan, verbose: bool, took: f64) {
    let how = match &plan.dest_scan {
        DestScan::Cached => tr("cache", "キャッシュ").to_string(),
        DestScan::Full(reason) => trf!("full listing ({reason})", "全一覧({reason})"),
    };
    println!(
        "{}",
        trf!(
            "  Compared in {took:.2} s / destination: {how}",
            "  比較: {took:.2}秒 / 保存先: {how}"
        )
    );
    let mib = plan.stats.copy_bytes as f64 / 1_048_576.0;
    println!(
        "{}",
        trf!(
            "  Source {} files, {} folders / unchanged {} / copy {} ({mib:.1} MiB) / delete {}",
            "  元 {} ファイル, {} フォルダ / 変更なし {} / コピー {} ({mib:.1} MiB) / 削除 {}",
            plan.stats.source_files,
            plan.stats.source_dirs,
            plan.stats.unchanged,
            plan.copies.len(),
            plan.delete_count(),
        )
    );
    for warning in &plan.warnings {
        println!("  {} {warning}", tr("Warning:", "警告:"));
    }
    if verbose {
        for item in &plan.copies {
            let mark = if item.is_new {
                tr("new", "新規")
            } else {
                tr("update", "更新")
            };
            println!("  {mark} {}", item.rel.display());
        }
        let delete = tr("delete", "削除");
        for rel in plan.conflict_files.iter().chain(&plan.delete_files) {
            println!("  {delete} {}", rel.display());
        }
        for rel in plan.conflict_dirs.iter().chain(&plan.delete_dirs) {
            println!("  {delete} {}/", rel.display());
        }
    }
}

/// Returns true when everything succeeded.
fn run_pair(
    spec: PairSpec,
    flags: &RunFlags,
    days: u32,
    ctx: &RunContext,
) -> Result<bool, MirrorError> {
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
        println!("  {} {reason}", tr("Safety check:", "安全装置:"));
        if !flags.dry_run && !flags.allow_delete {
            return Err(MirrorError::Guard(
                tr(
                    "Nothing was changed. Review the plan, then add --allow-delete if it is right.",
                    "何も変更していません。内容を確認して、問題なければ --allow-delete を付けてください",
                )
                .into(),
            ));
        }
    }
    if flags.dry_run {
        return Ok(plan.scan_errors == 0);
    }
    let started = Instant::now();
    let report: Report = exec::execute(plan, ctx, flags.allow_delete)?;
    let took = started.elapsed().as_secs_f64();
    let mib = report.copied_bytes as f64 / 1_048_576.0;
    println!(
        "{}",
        trf!(
            "  Done in {took:.2} s / copied {} ({mib:.1} MiB) / deleted {} / errors {}",
            "  完了: {took:.2}秒 / コピー {} ({mib:.1} MiB) / 削除 {} / エラー {}",
            report.copied,
            report.deleted,
            report.errors + report.scan_errors,
        )
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
                eprintln!("  {} {e}", tr("Stopped:", "中止:"));
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
    let saved = config::load(&config::config_path());
    let code = cli
        .lang
        .clone()
        .or_else(|| saved.as_ref().ok().map(|c| c.language.clone()));
    i18n::set_lang(code.map_or(Lang::En, |c| Lang::from_code(&c)));

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
            let as_file = Path::new(&pattern);
            let is_file = as_file
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(config::PATTERN_EXTENSION));
            if is_file {
                return match config::load_pattern_file(as_file) {
                    Ok(found) => run_all(
                        specs_from(&found.pairs, &found.excludes),
                        &flags,
                        found.full_check_days,
                    ),
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::from(1)
                    }
                };
            }
            let config = match saved {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(1);
                }
            };
            let Some(found) = config.patterns.iter().find(|p| p.name == pattern) else {
                eprintln!(
                    "{}",
                    trf!(
                        "Pattern not found: {pattern}",
                        "パターンが見つかりません: {pattern}"
                    )
                );
                return ExitCode::from(1);
            };
            run_all(
                specs_from(&found.pairs, &found.excludes),
                &flags,
                found.full_check_days,
            )
        }
        Command::List => match saved {
            Ok(config) => {
                for pattern in &config.patterns {
                    let count = pattern.pairs.len();
                    println!(
                        "{}",
                        trf!("{} ({count} pairs)", "{} ({count} 組)", pattern.name)
                    );
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
