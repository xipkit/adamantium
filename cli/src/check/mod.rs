mod baseline;
mod command;
mod diagnostic;
#[cfg(test)]
mod tests;
mod text;

use std::{
    collections::{BTreeMap, HashSet},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    lints::{self, EXPERIMENTAL_LINTS, OVERFLOW_RULE},
    manifest, sarif,
};

pub use self::diagnostic::normalize;
use self::{
    baseline::Baseline,
    command::{clippy_command, dylint_command, write_clippy_config},
    diagnostic::{Setting, Sorted, classify, parse_cargo_line},
    text::{findings_label, print_text},
};
pub use crate::status::Status;

const SCHEMA_VERSION: u32 = 3;

/// Arguments for `cargo adamantium check`.
#[derive(clap::Args)]
pub struct Args {
    #[arg(default_value = ".", help = "The project directory, or its Cargo.toml")]
    path: PathBuf,
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    format: ReportFormat,
    #[arg(
        long,
        help = "Also run the experimental lints from this explicitly built Dylint library"
    )]
    dylint_library: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "dylint_library",
        help = "Run the crate-wide checks using this release's verified prebuilt library"
    )]
    deep: bool,
    #[arg(
        long,
        help = "Include the strict rules for arithmetic and synchronization"
    )]
    strict: bool,
    #[arg(
        long,
        conflicts_with = "strict",
        help = "Disable the strict tier set in Cargo.toml"
    )]
    no_strict: bool,
    #[arg(
        long,
        value_name = "REPORT",
        help = "Leave out findings from an earlier `--format json` report"
    )]
    baseline: Option<PathBuf>,
    #[arg(
        long,
        help = "Remove findings that no longer occur from the given baseline file"
    )]
    update_baseline: bool,
    #[arg(
        long,
        help = "Exit with status 1 if the given baseline still has unmatched findings"
    )]
    fail_on_stale_baseline: bool,
    #[arg(
        long,
        help = "In text output, summarize findings by rule and by the 10 most affected files"
    )]
    summary: bool,
    #[arg(
        last = true,
        help = "Arguments passed on to `cargo clippy`, such as `--release` or `-p name`"
    )]
    cargo_args: Vec<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ReportFormat {
    #[value(help = "Clippy's own output, then a summary, on stderr")]
    Text,
    #[value(help = "A report on stdout")]
    Json,
    #[value(help = "A SARIF 2.1.0 log on stdout, for GitHub code scanning")]
    Sarif,
}

/// What a check found.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    pub status: Status,
    pub findings: Vec<Finding>,
    pub skipped: BTreeMap<String, usize>,
    pub baselined: BTreeMap<String, usize>,
    pub baseline_unmatched: Option<usize>,
    pub warnings: Vec<Problem>,
    pub errors: Vec<Problem>,
}

/// One broken rule at one place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Finding {
    pub rule: String,
    pub message: String,
    pub file: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// A compiler error or configuration warning unrelated to the rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Problem {
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl Problem {
    /// A problem that no file is the place of.
    fn unlocated(message: String) -> Self {
        Self {
            message,
            file: None,
            line: None,
        }
    }
}

/// Runs the check, applies any baseline options, and prints the report.
pub fn run(args: Args) -> Result<ExitCode> {
    let root = manifest::root(&args.path)?;
    let config = Config::load(&root)?;
    let baseline_path = args.baseline.as_deref().or(config.baseline.as_deref());
    if baseline_path.is_none() && (args.update_baseline || args.fail_on_stale_baseline) {
        let flag = if args.update_baseline {
            "--update-baseline"
        } else {
            "--fail-on-stale-baseline"
        };
        bail!("{flag} requires --baseline <REPORT> or a baseline in Cargo.toml");
    }
    let mut baseline = baseline_path.map(Baseline::load).transpose()?;
    let text = matches!(args.format, ReportFormat::Text);
    let mut report = check_selected(
        &root,
        &args.cargo_args,
        &config,
        CheckOptions {
            echo: text,
            strict: args.strict || (!args.no_strict && config.strict),
            baseline: baseline.as_mut(),
            dylint_library: args.dylint_library.as_deref(),
            deep: args.deep,
            ..CheckOptions::default()
        },
    )?;
    let pruned = if args.update_baseline
        && report.status != Status::Error
        && let (Some(path), Some(baseline)) = (baseline_path, baseline)
    {
        baseline.prune(path)?
    } else {
        0
    };
    if pruned > 0 {
        report.baseline_unmatched = Some(0);
    }
    match args.format {
        ReportFormat::Json => print_json(&report)?,
        ReportFormat::Sarif => print_json(&sarif::log(
            &report,
            &sarif::Roots::locate(&root, &args.cargo_args)?,
        ))?,
        ReportFormat::Text => print_text(&mut io::stderr().lock(), &report, args.summary)?,
    }
    if report.status == Status::Error {
        return Ok(Status::Error.exit_code());
    }
    let mut stderr = io::stderr().lock();
    if pruned > 0 {
        writeln!(
            stderr,
            "adamantium: removed {pruned} stale baseline {}",
            findings_label(pruned)
        )?;
    }
    if let Some(unmatched @ 1..) = report.baseline_unmatched
        && (text || args.fail_on_stale_baseline)
    {
        let verb = if unmatched == 1 { "occurs" } else { "occur" };
        writeln!(
            stderr,
            "adamantium: {unmatched} {} in the baseline no longer {verb}. Run with --update-baseline to prune the baseline.",
            findings_label(unmatched),
        )?;
        if args.fail_on_stale_baseline {
            return Ok(Status::Findings.exit_code());
        }
    }
    Ok(report.status.exit_code())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

pub(crate) fn check(
    root: &Path,
    cargo_args: &[String],
    echo: bool,
    strict: bool,
) -> Result<Report> {
    check_with_suggestions(root, cargo_args, echo, strict, None)
}

pub(crate) fn check_with_suggestions(
    root: &Path,
    cargo_args: &[String],
    echo: bool,
    strict: bool,
    diagnostics: Option<&mut Vec<serde_json::Value>>,
) -> Result<Report> {
    let config = Config::load(root)?;
    check_selected(
        root,
        cargo_args,
        &config,
        CheckOptions {
            echo,
            strict,
            diagnostics,
            ..CheckOptions::default()
        },
    )
}

#[derive(Default)]
struct CheckOptions<'a> {
    echo: bool,
    strict: bool,
    baseline: Option<&'a mut Baseline>,
    diagnostics: Option<&'a mut Vec<Value>>,
    dylint_library: Option<&'a Path>,
    deep: bool,
}

fn check_selected(
    root: &Path,
    cargo_args: &[String],
    config: &Config,
    options: CheckOptions<'_>,
) -> Result<Report> {
    let CheckOptions {
        echo,
        strict,
        baseline,
        mut diagnostics,
        dylint_library,
        deep,
    } = options;
    let conf = tempfile::tempdir().context("creating a directory for clippy.toml")?;
    let (clippy_config, clippy_toml) =
        write_clippy_config(conf.path(), strict, diagnostics.is_some())?;
    let reasons = lints::Reasons::parse(&clippy_toml);

    let mut commands = vec![clippy_command(root, conf.path(), cargo_args, strict)];
    let mut sorted = Sorted {
        baseline,
        ..Sorted::default()
    };
    // Keep the verified copy alive through both Dylint invocations.
    let prebuilt;
    let dylint = if deep {
        Some(match crate::deep::prepare(root) {
            Ok(prepared) => {
                prebuilt = prepared;
                dylint_command(root, cargo_args, &prebuilt.library, Some(&prebuilt.runner))
            }
            Err(error) => Err(error),
        })
    } else {
        dylint_library.map(|path| dylint_command(root, cargo_args, path, None))
    };
    if let Some(result) = dylint {
        match result {
            Ok(command) => commands.push(command),
            Err(error) => sorted.errors.push(Problem::unlocated(format!(
                "experimental check incomplete: {error:#}"
            ))),
        }
    }
    let mut rules: HashSet<String> = lints::selected(strict)
        .map(lints::Lint::qualified)
        .collect();
    if deep || dylint_library.is_some() {
        rules.extend(EXPERIMENTAL_LINTS.iter().map(|name| (*name).to_owned()));
    }
    let setting = Setting {
        root,
        clippy_config: &clippy_config,
        reasons: &reasons,
        config,
        rules: &rules,
        echo,
    };
    let mut complete = true;
    for command in commands {
        complete &= run_checker(command, &setting, &mut sorted, &mut diagnostics)?;
    }
    overflow_finding(root, config, &reasons, echo, &mut sorted)?;

    let Sorted {
        findings,
        skipped,
        baseline,
        baselined,
        warnings,
        errors,
        ..
    } = sorted;
    let status = if !errors.is_empty() || !complete {
        Status::Error
    } else if findings.is_empty() {
        Status::Clean
    } else {
        Status::Findings
    };
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        root: root.to_path_buf(),
        status,
        findings,
        skipped,
        baselined,
        baseline_unmatched: baseline.as_deref().map(Baseline::unmatched),
        warnings,
        errors,
    })
}

/// Runs one checker and sorts the diagnostics it prints. Returns whether it
/// started and finished its build. In fix mode, `diagnostics` collects the
/// raw diagnostic of each finding.
fn run_checker(
    mut command: Command,
    setting: &Setting<'_>,
    sorted: &mut Sorted<'_>,
    diagnostics: &mut Option<&mut Vec<Value>>,
) -> Result<bool> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            sorted
                .errors
                .push(Problem::unlocated(format!("check incomplete: {error}")));
            return Ok(false);
        }
    };
    let stdout = child.stdout.take().context("capturing cargo's output")?;
    let mut built = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading cargo's output")?;
        let Ok(parsed) = parse_cargo_line(&line) else {
            continue;
        };
        match parsed.reason.as_str() {
            "compiler-message" => {
                if let Some(diagnostic) = parsed.message {
                    let probe = diagnostics.is_some()
                        && diagnostic
                            .code
                            .as_ref()
                            .is_some_and(|code| code.code == "clippy::disallowed_methods")
                        && matches!(
                            lints::quoted_path(&diagnostic.message),
                            Some(lints::RWLOCK_READ | lints::RWLOCK_WRITE)
                        );
                    let before = sorted.findings.len();
                    if !probe {
                        classify(diagnostic, setting, sorted);
                    }
                    if (probe || sorted.findings.len() > before)
                        && let Some(diagnostics) = diagnostics.as_mut()
                        && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&line)
                        && let Some(message) = raw.get("message")
                    {
                        diagnostics.push(message.clone());
                    }
                }
            }
            "build-finished" => built = parsed.success.unwrap_or(false),
            _ => {}
        }
    }
    let exit = child.wait().context("waiting for checker")?;
    let finished = built && exit.success();
    if !finished {
        sorted.errors.push(Problem::unlocated(format!(
            "check incomplete: compiler or lint driver did not finish successfully ({exit})"
        )));
    }
    Ok(finished)
}

/// Reports a release profile that doesn't check for integer overflow, unless
/// the skip list or the baseline leaves the finding out.
fn overflow_finding(
    root: &Path,
    config: &Config,
    reasons: &lints::Reasons,
    echo: bool,
    sorted: &mut Sorted<'_>,
) -> Result<()> {
    let overflow_unchecked = !manifest::release_checks_overflow(root)?;
    if overflow_unchecked && config.skips(OVERFLOW_RULE, "Cargo.toml") {
        *sorted.skipped.entry(OVERFLOW_RULE.to_owned()).or_insert(0) += 1;
    } else if overflow_unchecked && !sorted.in_baseline(OVERFLOW_RULE, "Cargo.toml", None) {
        let message = "[profile.release] does not set overflow-checks = true, so integer overflow wraps silently in release builds; `cargo adamantium init` adds it".to_owned();
        let remediation = lints::remediation(OVERFLOW_RULE, &message, reasons);
        let finding = Finding {
            rule: OVERFLOW_RULE.to_owned(),
            message,
            file: "Cargo.toml".to_owned(),
            line: None,
            column: None,
            source: None,
            remediation,
        };
        if echo {
            eprintln!(
                "error: {}\n  --> Cargo.toml\n  = note: {}\n",
                finding.message, finding.rule
            );
        }
        sorted.findings.push(finding);
    }
    Ok(())
}
