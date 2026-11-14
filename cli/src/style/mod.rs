//! Judgment-based review of Rust files against the Google Rust style guide,
//! through a model provider, the OpenAI Decisions API by default. Formerly
//! the standalone `cargo rustoleum`; the rules, labeled cases and evaluation
//! scripts live under `style/` in the repository.

mod changed;
mod credentials;
mod decisions;
mod http;
mod provider;
mod report;
mod resume;
mod rules;
mod scan;

use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use self::{
    provider::ProviderKind,
    report::{Report, ReviewKey},
    resume::Cache,
    rules::RuleSet,
    scan::Scan,
};

#[derive(Subcommand)]
pub enum Command {
    /// Review a Rust file or directory without modifying it. Sends each file
    /// to the provider, which needs credentials: for the default Decisions
    /// API, OPENAI_API_KEY or, on macOS, an `openai-api-key` Keychain item.
    Check(CheckArgs),
    /// List the style rules.
    Rules {
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Args)]
pub struct CheckArgs {
    /// Rust source file or directory to review.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Evaluate only this rule; may be repeated.
    #[arg(long = "rule")]
    rules: Vec<String>,
    /// The service that answers the rules for each file.
    #[arg(long, value_enum, default_value_t = ProviderKind::Decisions)]
    provider: ProviderKind,
    /// Project context shared with the provider for each file.
    #[arg(long)]
    context: Option<String>,
    /// Show the files and rules that would be checked without an API call.
    #[arg(long)]
    dry_run: bool,
    /// Review only the Rust files that differ from BASE, or from HEAD without
    /// one: committed since the branch left BASE, staged, unstaged or
    /// untracked.
    #[arg(
        long,
        value_name = "BASE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "HEAD"
    )]
    changed: Option<String>,
    /// Reuse results from a previous JSON report for files whose source,
    /// provider, model, rules and context are unchanged.
    #[arg(long, value_name = "REPORT")]
    cache_from: Option<PathBuf>,
    /// Retry files that failed or were not checked in a previous JSON report.
    #[arg(long, conflicts_with_all = ["changed", "cache_from"])]
    retry_from: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Json,
    Text,
}

/// Runs a `style` subcommand. Exit status is 0 for a clean or planned
/// review, 1 for findings and 2 for an incomplete review.
pub fn run(command: Command) -> Result<ExitCode> {
    let rules = RuleSet::load()?;
    match command {
        Command::Rules { format } => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            match format {
                Format::Json => {
                    serde_json::to_writer_pretty(&mut output, &rules.questions)?;
                    writeln!(output)?;
                }
                Format::Text => {
                    for question in &rules.questions {
                        writeln!(output, "{}: {}", question.name, question.instructions)?;
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check(args) => {
            let questions = rules.select(&args.rules)?;
            let key = ReviewKey::new(
                args.provider,
                &rules.model,
                &questions,
                args.context.as_deref(),
            )?;
            let mut scan = Scan::discover(&args.path)?;
            if let Some(base) = &args.changed {
                changed::retain_changed(&mut scan, base)?;
            }
            if let Some(previous) = &args.retry_from {
                resume::select_pending(
                    &mut scan,
                    previous,
                    args.provider,
                    &rules.model,
                    &questions,
                )?;
            }
            let mut report = Report::planned(&scan, key, &questions);
            if let Some(previous) = &args.cache_from {
                Cache::load(previous, &scan, &report.key)?.apply(&mut report, &scan);
            }
            if !args.dry_run {
                if report.has_planned_files() {
                    let provider = args.provider.connect()?;
                    report.check(
                        &scan,
                        &questions,
                        args.context.as_deref(),
                        provider.as_ref(),
                    );
                } else {
                    report.settle();
                }
            }
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            match args.format {
                Format::Json => {
                    serde_json::to_writer_pretty(&mut output, &report)?;
                    writeln!(output)?;
                }
                Format::Text => render_text(&mut output, &report)?,
            }
            Ok(ExitCode::from(report.exit_code()))
        }
    }
}

fn render_text(output: &mut impl Write, report: &Report) -> Result<()> {
    writeln!(output, "{}: {}", report.status, report.root)?;
    for file in &report.files {
        let cached = if file.cached { " (cached)" } else { "" };
        writeln!(output, "{}: {}{cached}", file.path, file.status)?;
        for finding in &file.findings {
            match &finding.estimate {
                Some(estimate) => writeln!(
                    output,
                    "  {} (confidence {:.3}, file scope)",
                    finding.rule, estimate.confidence
                )?,
                None => writeln!(output, "  {} (file scope)", finding.rule)?,
            }
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
