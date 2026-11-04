//! Running Clippy with the rules and turning its output into a report.

use std::{
    collections::HashSet,
    env, fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    lints::{CLIPPY_TOML, LINTS},
    manifest,
};

/// The rule name for the release profile check, which is not a Clippy lint.
const OVERFLOW_RULE: &str = "adamantium::overflow_checks";

/// What a check found.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    pub status: Status,
    pub findings: Vec<Finding>,
    pub errors: Vec<Problem>,
}

/// The outcome of a check, which is also its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// No rule was broken.
    Clean,
    /// At least one rule was broken.
    Findings,
    /// The project did not build, so the rules could not all be checked.
    Error,
}

/// One broken rule at one place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Finding {
    pub rule: String,
    pub message: String,
    pub file: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

/// A compiler error unrelated to the rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Problem {
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
}

#[derive(Deserialize)]
struct CargoLine {
    reason: String,
    message: Option<Diagnostic>,
    success: Option<bool>,
}

#[derive(Deserialize)]
struct Diagnostic {
    message: String,
    level: String,
    code: Option<Code>,
    rendered: Option<String>,
    #[serde(default)]
    spans: Vec<Span>,
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u64,
    column_start: u64,
    is_primary: bool,
}

impl Status {
    fn exit_code(self) -> ExitCode {
        match self {
            Status::Clean => ExitCode::SUCCESS,
            Status::Findings => ExitCode::from(1),
            Status::Error => ExitCode::from(2),
        }
    }
}

/// Runs the check and prints the report, as text on stderr or as JSON on
/// stdout. The exit status is 0 when clean, 1 with findings and 2 when the
/// project failed to build.
pub fn run(path: &Path, cargo_args: &[String], json: bool) -> Result<ExitCode> {
    let root = manifest::root(path)?;
    let report = check(&root, cargo_args, !json)?;
    if json {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else {
        let mut stderr = io::stderr().lock();
        match report.status {
            Status::Clean => writeln!(stderr, "adamantium: clean")?,
            Status::Findings => {
                let mut files: Vec<&str> = report
                    .findings
                    .iter()
                    .map(|finding| finding.file.as_str())
                    .collect();
                files.sort_unstable();
                files.dedup();
                writeln!(
                    stderr,
                    "adamantium: {} finding{} in {}",
                    report.findings.len(),
                    if report.findings.len() == 1 { "" } else { "s" },
                    files.join(", ")
                )?;
            }
            Status::Error => writeln!(
                stderr,
                "adamantium: the project did not build, so the rules were not all checked"
            )?,
        }
    }
    Ok(report.status.exit_code())
}

fn check(root: &Path, cargo_args: &[String], echo: bool) -> Result<Report> {
    let conf = tempfile::tempdir().context("creating a directory for clippy.toml")?;
    fs::write(conf.path().join("clippy.toml"), CLIPPY_TOML)?;

    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root)
        .env("CLIPPY_CONF_DIR", conf.path())
        .args(["clippy", "--all-targets", "--message-format=json"]);
    // Cargo checks only the root package unless told otherwise, which would
    // skip every other workspace member.
    if !selects_packages(cargo_args) {
        command.arg("--workspace");
    }
    command
        .args(cargo_args)
        // Only adamantium's rules count; a project runs plain Clippy for the
        // rest. The rules run as warnings and every lint is capped at a
        // warning, so a crate with findings still builds and the crates and
        // test targets that depend on it are checked too. A lint the project
        // itself denies would otherwise stop the build at the first crate.
        .args(["--", "--cap-lints", "warn", "-A", "clippy::all"]);
    for lint in LINTS {
        command.args(["-W", &lint.qualified()]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().context("running cargo clippy")?;
    let stdout = child.stdout.take().context("capturing cargo's output")?;

    let rules: HashSet<String> = LINTS.iter().map(super::lints::Lint::qualified).collect();
    let mut findings = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut built = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading cargo's output")?;
        let Ok(parsed) = serde_json::from_str::<CargoLine>(&line) else {
            continue;
        };
        match parsed.reason.as_str() {
            "compiler-message" => {
                if let Some(diagnostic) = parsed.message {
                    classify(
                        diagnostic,
                        &rules,
                        echo,
                        &mut seen,
                        &mut findings,
                        &mut errors,
                    );
                }
            }
            "build-finished" => built = parsed.success.unwrap_or(false),
            _ => {}
        }
    }
    let exit = child.wait().context("waiting for cargo clippy")?;

    if !manifest::release_checks_overflow(root)? {
        let finding = Finding {
            rule: OVERFLOW_RULE.to_owned(),
            message: "[profile.release] does not set overflow-checks = true, so integer overflow wraps silently in release builds; `cargo adamantium init` adds it".to_owned(),
            file: "Cargo.toml".to_owned(),
            line: None,
            column: None,
        };
        if echo {
            eprintln!(
                "error: {}\n  --> Cargo.toml\n  = note: {}\n",
                finding.message, finding.rule
            );
        }
        findings.push(finding);
    }

    let status = if !errors.is_empty() || (findings.is_empty() && (!built || !exit.success())) {
        Status::Error
    } else if findings.is_empty() {
        Status::Clean
    } else {
        Status::Findings
    };
    Ok(Report {
        schema_version: 1,
        root: root.to_path_buf(),
        status,
        findings,
        errors,
    })
}

/// Whether the arguments for `cargo clippy` already choose which packages to
/// check, in which case adding `--workspace` would override that choice.
fn selects_packages(cargo_args: &[String]) -> bool {
    cargo_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-p" | "--package" | "--workspace" | "--all" | "--exclude"
        ) || arg.starts_with("--package=")
    })
}

/// Sorts one diagnostic into a finding, a build error, or noise, printing it
/// when `echo` is set. Diagnostics without a primary span, such as the
/// "aborting due to N errors" summary, are dropped.
fn classify(
    diagnostic: Diagnostic,
    rules: &HashSet<String>,
    echo: bool,
    seen: &mut HashSet<(String, String, u64, u64)>,
    findings: &mut Vec<Finding>,
    errors: &mut Vec<Problem>,
) {
    let Some(span) = diagnostic.spans.iter().find(|span| span.is_primary) else {
        return;
    };
    let code = diagnostic.code.as_ref().map(|code| code.code.clone());
    let key = (
        code.clone().unwrap_or_default(),
        span.file_name.clone(),
        span.line_start,
        span.column_start,
    );
    // Clippy reports a lint once per target, so a library and its tests
    // repeat each other.
    if !seen.insert(key) {
        return;
    }
    if echo && let Some(rendered) = &diagnostic.rendered {
        eprint!("{rendered}");
    }
    match code {
        Some(rule) if rules.contains(&rule) => findings.push(Finding {
            rule,
            message: diagnostic.message,
            file: span.file_name.clone(),
            line: Some(span.line_start),
            column: Some(span.column_start),
        }),
        _ if diagnostic.level == "error" => errors.push(Problem {
            message: diagnostic.message,
            file: Some(span.file_name.clone()),
            line: Some(span.line_start),
        }),
        _ => {}
    }
}
