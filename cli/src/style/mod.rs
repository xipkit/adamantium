//! Judgment-based review of Rust files against the Google Rust style guide,
//! through a model provider, the OpenAI Decisions API by default. Formerly
//! the standalone `cargo rustoleum`; the rules, labeled cases and evaluation
//! scripts live under `style/` in the repository.

mod changed;
mod decisions;
#[cfg(test)]
mod fake;
mod locate;
mod provider;
mod report;
mod resume;
mod rules;
mod scan;

use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use self::{
    provider::{Provider, ProviderKind},
    report::{Report, ReviewKey},
    resume::Cache,
    rules::{Pack, RuleSet},
    scan::Scan,
};

#[derive(Subcommand)]
pub enum Command {
    /// Review a Rust file or directory without modifying it. Sends each file
    /// to the provider, which needs credentials: for the default Decisions
    /// API, OPENAI_API_KEY or, on macOS, an `openai-api-key` Keychain item.
    Check(CheckArgs),
    /// List the rules of a pack.
    Rules {
        /// The rules to list.
        #[arg(long, value_enum, default_value_t = Pack::Google)]
        pack: Pack,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[derive(Args)]
pub struct CheckArgs {
    /// Rust source file or directory to review.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// The rules to review against.
    #[arg(long, value_enum, default_value_t = Pack::Google)]
    pack: Pack,
    /// Evaluate only this rule of the pack; may be repeated.
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
    /// provider, pack, model, rules and context are unchanged.
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
    match command {
        Command::Rules { pack, format } => {
            let rules = RuleSet::load(pack)?;
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
            let report = check(&args, || args.provider.connect())?;
            let stdout = std::io::stdout();
            write_report(&mut stdout.lock(), &report, args.format)?;
            Ok(ExitCode::from(report.exit_code()))
        }
    }
}

/// Plans the review `args` describe and, unless it is a dry run, sends each
/// planned file to the provider that `connect` builds. `connect` is called
/// only when some file needs a provider.
fn check(args: &CheckArgs, connect: impl FnOnce() -> Result<Box<dyn Provider>>) -> Result<Report> {
    let rules = RuleSet::load(args.pack)?;
    let questions = rules.select(&args.rules)?;
    let key = ReviewKey::new(
        args.provider,
        args.pack,
        &rules.model,
        &questions,
        args.context.as_deref(),
    )?;
    let mut scan = Scan::discover(&args.path)?;
    if let Some(base) = &args.changed {
        changed::retain_changed(&mut scan, base)?;
    }
    if let Some(previous) = &args.retry_from {
        resume::select_pending(&mut scan, previous, &key, &questions)?;
    }
    let mut report = Report::planned(&scan, key, &questions);
    if let Some(previous) = &args.cache_from {
        Cache::load(previous, &scan, &report.key)?.apply(&mut report, &scan);
    }
    if !args.dry_run {
        if report.has_planned_files() {
            let provider = connect()?;
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
    Ok(report)
}

fn write_report(output: &mut impl Write, report: &Report, format: Format) -> Result<()> {
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut *output, report)?;
            writeln!(output)?;
        }
        Format::Text => render_text(output, report)?,
    }
    Ok(())
}

fn render_text(output: &mut impl Write, report: &Report) -> Result<()> {
    writeln!(output, "{}: {}", report.status, report.root)?;
    for file in &report.files {
        let cached = if file.cached { " (cached)" } else { "" };
        writeln!(output, "{}: {}{cached}", file.path, file.status)?;
        for finding in &file.findings {
            let place = match finding.lines {
                Some(lines) if lines.start_line == lines.end_line => {
                    format!("line {}", lines.start_line)
                }
                Some(lines) => format!("lines {}-{}", lines.start_line, lines.end_line),
                None => "file scope".to_owned(),
            };
            writeln!(
                output,
                "  {} (confidence {:.3}, {place})",
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clap::Parser;

    use super::*;
    use crate::style::{
        fake::{FakeLocations, FakeProvider},
        provider::{LineRange, Verdict},
        report::{FileStatus, Finding},
    };

    /// `style check` as the command line parses it.
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: CheckArgs,
    }

    /// The JSON and the text after its header line that `style check` prints
    /// for `dir` with `options`, answered by `provider`, and its exit status.
    fn review(
        dir: &std::path::Path,
        options: &[&str],
        provider: FakeProvider,
    ) -> Result<(serde_json::Value, String, u8)> {
        let mut command = vec!["style-check".into(), dir.as_os_str().to_owned()];
        command.extend(options.iter().map(Into::into));
        let args = Cli::try_parse_from(command)?.args;
        let report = check(&args, || Ok(Box::new(provider)))?;
        let mut json = Vec::new();
        write_report(&mut json, &report, Format::Json)?;
        let mut text = Vec::new();
        write_report(&mut text, &report, Format::Text)?;
        let text = String::from_utf8(text)?;
        let (header, body) = text.split_once('\n').unwrap_or_default();
        anyhow::ensure!(header == format!("{}: {}", report.status, report.root));
        Ok((
            serde_json::from_slice(&json)?,
            body.to_owned(),
            report.exit_code(),
        ))
    }

    /// Two balances that should move together, in separate Actors.
    const SPLIT_STATE: &str = "struct Pair {\n    a: Actor<u64>,\n    b: Actor<u64>,\n}\n";

    #[test]
    fn a_concurrency_finding_says_what_goes_wrong_and_where() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), SPLIT_STATE).unwrap();
        let provider = FakeProvider {
            locations: FakeLocations::Fixed(vec![Some(LineRange {
                start_line: 1,
                end_line: 4,
            })]),
            ..FakeProvider::answering(&[Verdict::Violation, Verdict::NoViolation])
        };
        let (json, text, exit) = review(dir.path(), &["--pack", "concurrency"], provider).unwrap();
        let explanation = "Coupled mutable state with a joint invariant is split across two or \
                           more separate Actors, locks, or atomics and accessed in separate \
                           critical sections.";
        assert_eq!(exit, 1);
        assert_eq!(json["status"], "complete");
        assert_eq!(json["pack"], "concurrency");
        let file = &json["files"][0];
        assert_eq!(file["path"], "lib.rs");
        assert_eq!(file["status"], "checked");
        assert_eq!(file["findings"][0]["rule"], "split_actor_invariant");
        assert_eq!(
            file["findings"][0]["lines"],
            serde_json::json!({ "start_line": 1, "end_line": 4 })
        );
        assert_eq!(file["findings"][0]["guidance"], explanation);
        assert_eq!(
            text,
            format!(
                "lib.rs: checked\n  \
                 split_actor_invariant (confidence 0.800, lines 1-4)\n    {explanation}\n"
            )
        );
    }

    #[test]
    fn a_clean_concurrency_review_has_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), SPLIT_STATE).unwrap();
        let provider = FakeProvider::answering(&[Verdict::NoViolation, Verdict::NoViolation]);
        let (json, text, exit) = review(dir.path(), &["--pack", "concurrency"], provider).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(json["status"], "complete");
        assert_eq!(json["files"][0]["status"], "checked");
        assert_eq!(json["files"][0]["findings"], serde_json::json!([]));
        assert_eq!(json["files"][0]["no_violation_count"], 2);
        assert_eq!(text, "lib.rs: checked\n");
    }

    #[test]
    fn a_provider_failure_leaves_the_review_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), SPLIT_STATE).unwrap();
        std::fs::write(dir.path().join("b.rs"), SPLIT_STATE).unwrap();
        let (json, text, exit) = review(
            dir.path(),
            &["--pack", "concurrency"],
            FakeProvider::unavailable(),
        )
        .unwrap();
        assert_eq!(exit, 2);
        assert_eq!(json["status"], "incomplete");
        assert_eq!(json["files"][0]["status"], "error");
        assert_eq!(json["files"][0]["error"], "service unavailable");
        assert_eq!(json["files"][1]["status"], "not_checked");
        assert_eq!(
            text,
            "a.rs: error\n  error: service unavailable\nb.rs: not_checked\n"
        );
    }

    #[test]
    fn text_output_shows_each_finding_s_lines_or_file_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let scan = Scan::discover(dir.path()).unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
        let key = ReviewKey::new(
            ProviderKind::Decisions,
            Pack::Google,
            &rules.model,
            &[],
            None,
        )
        .unwrap();
        let mut report = Report::planned(&scan, key, &[]);
        let finding = |rule: &str, lines| Finding {
            rule: rule.to_owned(),
            lines,
            guidance: format!("Fix {rule}."),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
        };
        let range = |start_line, end_line| {
            Some(LineRange {
                start_line,
                end_line,
            })
        };
        report.files[0].status = FileStatus::Checked;
        report.files[0].findings = vec![
            finding("spanning", range(4, 6)),
            finding("single", range(9, 9)),
            finding("whole", None),
        ];
        report.settle();
        let mut output = Vec::new();
        render_text(&mut output, &report).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(
            text.split_once('\n').unwrap().1,
            "lib.rs: checked\n  \
             spanning (confidence 0.900, lines 4-6)\n    Fix spanning.\n  \
             single (confidence 0.900, line 9)\n    Fix single.\n  \
             whole (confidence 0.900, file scope)\n    Fix whole.\n"
        );
    }
}
