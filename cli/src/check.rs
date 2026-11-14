//! Running Clippy with the rules and turning its output into a report.

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{self, BufRead, BufReader, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    ReportFormat,
    config::Config,
    lints::{self, CLIPPY_TOML, OVERFLOW_RULE},
    manifest, sarif,
};

/// What a check found.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    pub status: Status,
    pub findings: Vec<Finding>,
    /// Findings the project's skip list left out, counted by rule.
    pub skipped: BTreeMap<String, usize>,
    /// Warnings about Clippy configuration or unavailable lints.
    pub warnings: Vec<Problem>,
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
    /// The source line the finding starts on, without its indentation or
    /// trailing whitespace, or `None` when the finding has no line.
    pub source: Option<String>,
}

/// A compiler error or configuration warning unrelated to the rules.
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
    /// The source lines the span covers, starting with `line_start`.
    #[serde(default)]
    text: Vec<SpanLine>,
}

#[derive(Deserialize)]
struct SpanLine {
    text: String,
}

#[derive(PartialEq, Eq, Hash)]
enum DiagnosticKey {
    Spanned {
        code: Option<String>,
        level: String,
        message: String,
        file: String,
        line: u64,
        column: u64,
    },
    Spanless {
        code: String,
        message: String,
    },
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

/// Runs the check and prints the report, as text on stderr or as JSON or
/// SARIF on stdout. The exit status is 0 when clean, 1 with findings and 2
/// when the project failed to build.
pub fn run(
    path: &Path,
    cargo_args: &[String],
    format: ReportFormat,
    strict: bool,
) -> Result<ExitCode> {
    let root = manifest::root(path)?;
    let report = check(
        &root,
        cargo_args,
        matches!(format, ReportFormat::Text),
        strict,
    )?;
    match format {
        ReportFormat::Json => print_json(&report)?,
        ReportFormat::Sarif => print_json(&sarif::log(
            &report,
            &sarif::Roots::locate(&root, cargo_args)?,
        ))?,
        ReportFormat::Text => {
            let mut stderr = io::stderr().lock();
            let skipped: usize = report.skipped.values().sum();
            let note = if skipped == 0 {
                String::new()
            } else {
                format!(" ({skipped} skipped by the skip list in Cargo.toml)")
            };
            match report.status {
                Status::Clean => writeln!(stderr, "adamantium: clean{note}")?,
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
                        "adamantium: {} finding{} in {}{note}",
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
    }
    Ok(report.status.exit_code())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

/// Writes Adamantium's `clippy.toml` into `dir`, which becomes
/// `CLIPPY_CONF_DIR`, and returns the path Clippy's diagnostics will name it
/// by. Clippy canonicalizes `CLIPPY_CONF_DIR` before reading it, so when `dir`
/// is reached through a symlink, as macOS's temporary directory is through
/// `/var`, the two spellings differ.
fn write_clippy_config(dir: &Path) -> Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let path = dir.join("clippy.toml");
    fs::write(&path, CLIPPY_TOML).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn check(root: &Path, cargo_args: &[String], echo: bool, strict: bool) -> Result<Report> {
    let config = Config::load(root)?;
    let conf = tempfile::tempdir().context("creating a directory for clippy.toml")?;
    let clippy_config = write_clippy_config(conf.path())?;

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
    for lint in lints::selected(strict) {
        command.args(["-W", &lint.qualified()]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().context("running cargo clippy")?;
    let stdout = child.stdout.take().context("capturing cargo's output")?;

    let rules: HashSet<String> = lints::selected(strict)
        .map(lints::Lint::qualified)
        .collect();
    let setting = Setting {
        root,
        clippy_config: &clippy_config,
        config: &config,
        rules: &rules,
        echo,
    };
    let mut sorted = Sorted::default();
    let mut built = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading cargo's output")?;
        let Ok(parsed) = serde_json::from_str::<CargoLine>(&line) else {
            continue;
        };
        match parsed.reason.as_str() {
            "compiler-message" => {
                if let Some(diagnostic) = parsed.message {
                    classify(diagnostic, &setting, &mut sorted);
                }
            }
            "build-finished" => built = parsed.success.unwrap_or(false),
            _ => {}
        }
    }
    let exit = child.wait().context("waiting for cargo clippy")?;

    if !manifest::release_checks_overflow(root)? && config.skips(OVERFLOW_RULE, "Cargo.toml") {
        *sorted.skipped.entry(OVERFLOW_RULE.to_owned()).or_insert(0) += 1;
    } else if !manifest::release_checks_overflow(root)? {
        let finding = Finding {
            rule: OVERFLOW_RULE.to_owned(),
            message: "[profile.release] does not set overflow-checks = true, so integer overflow wraps silently in release builds; `cargo adamantium init` adds it".to_owned(),
            file: "Cargo.toml".to_owned(),
            line: None,
            column: None,
            source: None,
        };
        if echo {
            eprintln!(
                "error: {}\n  --> Cargo.toml\n  = note: {}\n",
                finding.message, finding.rule
            );
        }
        sorted.findings.push(finding);
    }

    let Sorted {
        findings,
        skipped,
        warnings,
        errors,
        ..
    } = sorted;
    let status = if !errors.is_empty() || (findings.is_empty() && (!built || !exit.success())) {
        Status::Error
    } else if findings.is_empty() {
        Status::Clean
    } else {
        Status::Findings
    };
    Ok(Report {
        schema_version: 3,
        root: root.to_path_buf(),
        status,
        findings,
        skipped,
        warnings,
        errors,
    })
}

/// What `classify` needs to know about the run.
struct Setting<'a> {
    root: &'a Path,
    clippy_config: &'a Path,
    config: &'a Config,
    rules: &'a HashSet<String>,
    echo: bool,
}

/// The diagnostics `classify` has sorted so far.
#[derive(Default)]
struct Sorted {
    /// Diagnostics already sorted, since Clippy repeats them across targets.
    seen: HashSet<DiagnosticKey>,
    findings: Vec<Finding>,
    skipped: BTreeMap<String, usize>,
    warnings: Vec<Problem>,
    errors: Vec<Problem>,
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

/// Sorts one diagnostic into a finding, a skipped finding, a build error, a
/// configuration or unavailable-lint warning, or noise, echoing diagnostics
/// when `echo` is set. Without a primary span, only unknown or removed lint
/// warnings are kept; compiler summaries are dropped.
fn classify(diagnostic: Diagnostic, setting: &Setting<'_>, sorted: &mut Sorted) {
    let Some(span) = diagnostic.spans.iter().find(|span| span.is_primary) else {
        let Some(code) = diagnostic.code else {
            return;
        };
        if diagnostic.level != "warning"
            || !matches!(code.code.as_str(), "E0602" | "renamed_and_removed_lints")
        {
            return;
        }
        if !sorted.seen.insert(DiagnosticKey::Spanless {
            code: code.code,
            message: diagnostic.message.clone(),
        }) {
            return;
        }
        if setting.echo
            && let Some(rendered) = &diagnostic.rendered
        {
            eprint!("{rendered}");
        }
        sorted.warnings.push(Problem {
            message: diagnostic.message,
            file: None,
            line: None,
        });
        return;
    };
    let about_config = Path::new(&span.file_name) == setting.clippy_config;
    let file = normalize(&span.file_name);
    let code = diagnostic.code.as_ref().map(|code| code.code.clone());
    let key = DiagnosticKey::Spanned {
        code: code.clone(),
        level: diagnostic.level.clone(),
        message: diagnostic.message.clone(),
        file: file.clone(),
        line: span.line_start,
        column: span.column_start,
    };
    // Clippy reports a lint once per target, so a library and its tests
    // repeat each other.
    if !sorted.seen.insert(key) {
        return;
    }
    if let Some(rule) = &code
        && setting.rules.contains(rule)
        && setting.config.skips(rule, relative(setting.root, &file))
    {
        *sorted.skipped.entry(rule.clone()).or_insert(0) += 1;
        return;
    }
    if setting.echo
        && let Some(rendered) = &diagnostic.rendered
    {
        eprint!("{rendered}");
    }
    let file = if about_config {
        "clippy.toml".to_owned()
    } else {
        file
    };
    match code {
        Some(rule) if setting.rules.contains(&rule) => sorted.findings.push(Finding {
            rule,
            message: diagnostic.message,
            file,
            line: Some(span.line_start),
            column: Some(span.column_start),
            source: span.text.first().map(|line| line.text.trim().to_owned()),
        }),
        _ if diagnostic.level == "error" => sorted.errors.push(Problem {
            message: diagnostic.message,
            file: Some(file),
            line: Some(span.line_start),
        }),
        _ if about_config && diagnostic.level == "warning" => sorted.warnings.push(Problem {
            message: diagnostic.message,
            file: Some(file),
            line: Some(span.line_start),
        }),
        _ => {}
    }
}

/// `file` relative to `root`, for the skip list. Clippy already names
/// workspace files relative to the directory it runs in.
fn relative<'a>(root: &Path, file: &'a str) -> &'a str {
    Path::new(file)
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or(file)
}

/// Resolves `.` and `..` segments without touching the file system, so a
/// file that two targets include under different spellings, such as
/// `tests/../examples/shared.rs`, is reported once under one name.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(parts.last(), Some(Component::Normal(_))) => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts
        .iter()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lints::Lint;

    fn diagnostic(file: &str, level: &str, code: Option<&str>, message: &str) -> Diagnostic {
        Diagnostic {
            message: message.to_owned(),
            level: level.to_owned(),
            code: code.map(|code| Code {
                code: code.to_owned(),
            }),
            rendered: None,
            spans: vec![Span {
                file_name: file.to_owned(),
                line_start: 7,
                column_start: 3,
                is_primary: true,
                text: vec![SpanLine {
                    text: "    let value = option.unwrap();  ".to_owned(),
                }],
            }],
        }
    }

    fn classify_diagnostics(diagnostics: Vec<Diagnostic>) -> Report {
        let root = Path::new("project");
        let config = Config::default();
        let rules = lints::selected(false).map(Lint::qualified).collect();
        let setting = Setting {
            root,
            clippy_config: Path::new("generated/clippy.toml"),
            config: &config,
            rules: &rules,
            echo: false,
        };
        let mut sorted = Sorted::default();
        for diagnostic in diagnostics {
            classify(diagnostic, &setting, &mut sorted);
        }
        Report {
            schema_version: 3,
            root: root.to_path_buf(),
            status: Status::Clean,
            findings: sorted.findings,
            skipped: sorted.skipped,
            warnings: sorted.warnings,
            errors: sorted.errors,
        }
    }

    #[test]
    fn config_warnings_preserve_distinct_messages_and_deduplicate_targets() {
        let report = classify_diagnostics(vec![
            diagnostic("generated/clippy.toml", "warning", None, "unresolved path"),
            diagnostic("generated/clippy.toml", "warning", None, "unresolved path"),
            diagnostic("generated/clippy.toml", "warning", None, "unknown key"),
            diagnostic(
                "generated/clippy.toml",
                "warning",
                Some("future_config_warning"),
                "a config warning with a code",
            ),
        ]);
        let report = serde_json::to_value(report).unwrap();
        assert_eq!(
            report["warnings"],
            serde_json::json!([
                { "message": "unresolved path", "file": "clippy.toml", "line": 7 },
                { "message": "unknown key", "file": "clippy.toml", "line": 7 },
                { "message": "a config warning with a code", "file": "clippy.toml", "line": 7 },
            ])
        );
        assert_eq!(report["findings"], serde_json::json!([]));
        assert_eq!(report["errors"], serde_json::json!([]));
    }

    fn spanless(level: &str, code: Option<&str>, message: &str) -> Diagnostic {
        let mut diagnostic = diagnostic("", level, code, message);
        diagnostic.spans.clear();
        diagnostic
    }

    #[test]
    fn spanless_lint_warnings_deduplicate_by_code_and_message() {
        let report = classify_diagnostics(vec![
            spanless("warning", Some("E0602"), "lint unavailable"),
            spanless("warning", Some("E0602"), "lint unavailable"),
            spanless("warning", Some("E0602"), "another lint unavailable"),
            spanless(
                "warning",
                Some("renamed_and_removed_lints"),
                "lint unavailable",
            ),
            spanless(
                "warning",
                Some("renamed_and_removed_lints"),
                "lint unavailable",
            ),
        ]);
        let report = serde_json::to_value(report).unwrap();
        assert_eq!(
            report["warnings"],
            serde_json::json!([
                { "message": "lint unavailable", "file": null, "line": null },
                { "message": "another lint unavailable", "file": null, "line": null },
                { "message": "lint unavailable", "file": null, "line": null },
            ])
        );
        assert_eq!(report["findings"], serde_json::json!([]));
        assert_eq!(report["errors"], serde_json::json!([]));
    }

    #[test]
    fn other_spanless_diagnostics_are_dropped() {
        let report = classify_diagnostics(vec![
            spanless("warning", None, "2 warnings emitted"),
            spanless("error", None, "aborting due to 2 previous errors"),
            spanless("warning", Some("unused_imports"), "unused import"),
            spanless("warning", Some("clippy::unwrap_used"), "unwrap used"),
            spanless("error", Some("E0602"), "unknown lint"),
            spanless("note", Some("E0602"), "unknown lint"),
            spanless("error", Some("renamed_and_removed_lints"), "removed lint"),
            spanless("note", Some("renamed_and_removed_lints"), "removed lint"),
        ]);
        assert!(report.warnings.is_empty());
        assert!(report.errors.is_empty());
        assert!(report.findings.is_empty());
    }

    #[test]
    fn only_primary_spans_in_the_generated_config_produce_warnings() {
        let mut without_primary = diagnostic("generated/clippy.toml", "warning", None, "summary");
        without_primary.spans.clear();
        let mut secondary = diagnostic("generated/clippy.toml", "warning", None, "secondary");
        secondary.spans.first_mut().unwrap().is_primary = false;
        let report = classify_diagnostics(vec![
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("unused_imports"),
                "unused import",
            ),
            diagnostic("project/clippy.toml", "warning", None, "another config"),
            diagnostic("generated/clippy.toml", "note", None, "a note"),
            without_primary,
            secondary,
        ]);
        assert!(report.warnings.is_empty());
        assert!(report.errors.is_empty());
        assert!(report.findings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn clippy_config_is_named_by_its_canonical_path() {
        let real = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("conf");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let path = write_clippy_config(&link).unwrap();
        assert_eq!(
            path,
            real.path().canonicalize().unwrap().join("clippy.toml")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), CLIPPY_TOML);
    }

    #[test]
    fn config_errors_keep_their_level_and_use_a_stable_file_name() {
        let report = classify_diagnostics(vec![
            diagnostic("generated/clippy.toml", "warning", None, "config problem"),
            diagnostic("generated/clippy.toml", "error", None, "config problem"),
            diagnostic("src/lib.rs", "error", Some("E0001"), "compile error"),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::unwrap_used"),
                "unwrap used",
            ),
        ]);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.errors,
            vec![
                Problem {
                    message: "config problem".to_owned(),
                    file: Some("clippy.toml".to_owned()),
                    line: Some(7),
                },
                Problem {
                    message: "compile error".to_owned(),
                    file: Some("src/lib.rs".to_owned()),
                    line: Some(7),
                },
            ]
        );
        assert_eq!(report.findings.len(), 1);
        let finding = report.findings.first().unwrap();
        assert_eq!(finding.rule, "clippy::unwrap_used");
        assert_eq!(
            finding.source.as_deref(),
            Some("let value = option.unwrap();")
        );
    }
}
