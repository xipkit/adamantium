mod api;
mod report;
mod resume;
mod rules;
mod scan;

use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{report::Report, rules::RuleSet, scan::Scan};

#[derive(Parser)]
#[command(
    name = "cargo rustoleum",
    version,
    about = "Review Rust source using the Decisions API"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a Rust file or directory without modifying it.
    Check(CheckArgs),
    /// List the available rules.
    Rules {
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Args)]
struct CheckArgs {
    /// Rust source file or directory to review.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Evaluate only this rule; may be repeated.
    #[arg(long = "rule")]
    rules: Vec<String>,
    /// Project context shared with the Decisions API for each file.
    #[arg(long)]
    context: Option<String>,
    /// Show the files and rules that would be checked without an API call.
    #[arg(long)]
    dry_run: bool,
    /// Retry files that failed or were not checked in a previous JSON report.
    #[arg(long)]
    retry_from: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Text,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("rustoleum: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|argument| argument == "rustoleum") {
        args.remove(1);
    }
    let cli = Cli::parse_from(args);
    let rules = RuleSet::load()?;
    match cli.command {
        Command::Rules { format } => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            match format {
                Format::Json => serde_json::to_writer_pretty(&mut output, &rules.questions)?,
                Format::Text => {
                    for question in &rules.questions {
                        writeln!(output, "{}: {}", question.name, question.instructions)?;
                    }
                    return Ok(0);
                }
            }
            writeln!(output)?;
            Ok(0)
        }
        Command::Check(args) => {
            let questions = rules.select(&args.rules)?;
            let mut scan = Scan::discover(&args.path)?;
            if let Some(previous) = &args.retry_from {
                resume::select_pending(&mut scan, previous, &rules.model, &questions)?;
            }
            let report = if args.dry_run {
                Report::planned(&scan, &rules.model, &questions)
            } else if scan.files.is_empty() {
                let mut report = Report::planned(&scan, &rules.model, &questions);
                report.status = "complete";
                report
            } else {
                let client = api::HttpDecisions::new(api::api_key()?);
                Report::check(
                    &scan,
                    &rules.model,
                    &questions,
                    args.context.as_deref(),
                    &client,
                )
            };
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            match args.format {
                Format::Json => {
                    serde_json::to_writer_pretty(&mut output, &report)?;
                    writeln!(output)?;
                }
                Format::Text => render_text(&mut output, &report)?,
            }
            Ok(report.exit_code())
        }
    }
}

fn render_text(output: &mut impl Write, report: &Report) -> Result<()> {
    writeln!(output, "{}: {}", report.status, report.root)?;
    for file in &report.files {
        writeln!(output, "{}: {}", file.path, file.status)?;
        for finding in &file.findings {
            writeln!(
                output,
                "  {} (confidence {:.3}, file scope)",
                finding.rule, finding.confidence
            )?;
            writeln!(output, "    {}", finding.guidance)?;
        }
        for unresolved in &file.unresolved {
            writeln!(output, "  {}: {}", unresolved.rule, unresolved.reason)?;
        }
        if let Some(error) = &file.error {
            writeln!(output, "  error: {error}")?;
        }
    }
    Ok(())
}
