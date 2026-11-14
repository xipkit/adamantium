//! Running Clippy with the rules and turning its output into a report.

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{self, BufRead, BufReader, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    ReportFormat,
    config::Config,
    lints::{self, CLIPPY_TOML, OVERFLOW_RULE},
    manifest, sarif,
};

/// The version of the JSON report's layout.
const SCHEMA_VERSION: u32 = 3;

/// What a check found.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    pub status: Status,
    pub findings: Vec<Finding>,
    /// Findings the project's skip list left out, counted by rule.
    pub skipped: BTreeMap<String, usize>,
    /// Findings the baseline matched, so they were left out, counted by rule.
    pub baselined: BTreeMap<String, usize>,
    /// How many of the baseline's findings this check did not match, because
    /// they were fixed or their line changed, or `None` without a baseline.
    pub baseline_unmatched: Option<usize>,
    /// Warnings about Clippy configuration or unavailable lints.
    pub warnings: Vec<Problem>,
    pub errors: Vec<Problem>,
}

/// The outcome of a check, which is also its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// The findings of an earlier JSON report, which a check leaves out so that
/// only new findings fail it. A finding is known by its rule, its path
/// relative to the project root and the text of its line, so it stays matched
/// when edits elsewhere move it. Findings that share all three are counted,
/// and only those beyond the earlier count are new.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    /// The earlier findings this check has not matched yet.
    remaining: BTreeMap<BaselineKey, usize>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BaselineKey {
    rule: String,
    path: String,
    source: Option<String>,
}

/// The parts of an earlier report that a baseline uses.
#[derive(Deserialize)]
struct EarlierReport {
    root: PathBuf,
    status: Status,
    findings: Vec<EarlierFinding>,
}

#[derive(Deserialize)]
struct EarlierFinding {
    rule: String,
    file: String,
    source: Option<String>,
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
    expansion: Option<Box<Expansion>>,
}

#[derive(Deserialize)]
struct Expansion {
    span: Span,
}

impl Span {
    fn call_site(&self) -> &Self {
        let mut span = self;
        while let Some(expansion) = &span.expansion {
            span = &expansion.span;
        }
        span
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        // Deep macro backtraces must also be dropped without recursive stack use.
        let mut expansion = self.expansion.take();
        while let Some(mut caller) = expansion {
            expansion = caller.span.expansion.take();
        }
    }
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

impl Baseline {
    /// Reads a report written by `check --format json`.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("using {} as a baseline", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let report: serde_json::Value =
            serde_json::from_str(text).context("it is not a JSON report")?;
        let version = report
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(SCHEMA_VERSION)) {
            let found = version.map_or_else(
                || "no schema version".to_owned(),
                |version| format!("schema version {version}"),
            );
            bail!(
                "it has {found}, but a baseline must be a schema version {SCHEMA_VERSION} report; regenerate it with `cargo adamantium check --format json`"
            );
        }
        let report: EarlierReport =
            serde_json::from_value(report).context("it is not a check report")?;
        if report.status == Status::Error {
            bail!("the project did not build when it was written, so its findings are incomplete");
        }
        let mut baseline = Self::default();
        for finding in report.findings {
            let key = BaselineKey {
                path: portable_path(&report.root, &finding.file),
                rule: finding.rule,
                source: finding.source,
            };
            *baseline.remaining.entry(key).or_insert(0) += 1;
        }
        Ok(baseline)
    }

    /// Matches a finding against one of the earlier findings not yet
    /// matched, returning whether there was one.
    fn take(&mut self, rule: &str, path: &str, source: Option<&str>) -> bool {
        let key = BaselineKey {
            rule: rule.to_owned(),
            path: path.to_owned(),
            source: source.map(str::to_owned),
        };
        match self.remaining.get_mut(&key) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        }
    }

    /// How many earlier findings were never matched.
    fn unmatched(&self) -> usize {
        self.remaining.values().sum()
    }
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
/// SARIF on stdout. Findings in `baseline`, an earlier JSON report, are left
/// out. The exit status is 0 when clean, 1 with findings and 2 when the
/// project failed to build.
pub fn run(
    path: &Path,
    cargo_args: &[String],
    format: ReportFormat,
    strict: bool,
    baseline: Option<&Path>,
) -> Result<ExitCode> {
    let root = manifest::root(path)?;
    let baseline = baseline.map(Baseline::load).transpose()?;
    let report = check(
        &root,
        cargo_args,
        matches!(format, ReportFormat::Text),
        strict,
        baseline,
    )?;
    match format {
        ReportFormat::Json => print_json(&report)?,
        ReportFormat::Sarif => print_json(&sarif::log(
            &report,
            &sarif::Roots::locate(&root, cargo_args)?,
        ))?,
        ReportFormat::Text => {
            let mut stderr = io::stderr().lock();
            let note = left_out_note(&report);
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
            match report.baseline_unmatched {
                None | Some(0) => {}
                Some(1) => writeln!(
                    stderr,
                    "adamantium: 1 finding in the baseline no longer occurs; regenerate the baseline so it can't come back"
                )?,
                Some(unmatched) => writeln!(
                    stderr,
                    "adamantium: {unmatched} findings in the baseline no longer occur; regenerate the baseline so they can't come back"
                )?,
            }
        }
    }
    Ok(report.status.exit_code())
}

/// The summary's note on findings the skip list or the baseline left out,
/// such as ` (3 skipped by the skip list in Cargo.toml, 2 in the baseline)`.
fn left_out_note(report: &Report) -> String {
    let skipped: usize = report.skipped.values().sum();
    let baselined: usize = report.baselined.values().sum();
    let mut parts = Vec::new();
    if skipped > 0 {
        parts.push(format!("{skipped} skipped by the skip list in Cargo.toml"));
    }
    if baselined > 0 {
        parts.push(format!("{baselined} in the baseline"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
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

fn check(
    root: &Path,
    cargo_args: &[String],
    echo: bool,
    strict: bool,
    baseline: Option<Baseline>,
) -> Result<Report> {
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
    let mut sorted = Sorted {
        baseline,
        ..Sorted::default()
    };
    let mut built = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("reading cargo's output")?;
        let Ok(parsed) = parse_cargo_line(&line) else {
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

    let overflow_unchecked = !manifest::release_checks_overflow(root)?;
    if overflow_unchecked && config.skips(OVERFLOW_RULE, "Cargo.toml") {
        *sorted.skipped.entry(OVERFLOW_RULE.to_owned()).or_insert(0) += 1;
    } else if overflow_unchecked && !sorted.in_baseline(OVERFLOW_RULE, "Cargo.toml", None) {
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
        baseline,
        baselined,
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
        schema_version: SCHEMA_VERSION,
        root: root.to_path_buf(),
        status,
        findings,
        skipped,
        baselined,
        baseline_unmatched: baseline.as_ref().map(Baseline::unmatched),
        warnings,
        errors,
    })
}

fn parse_cargo_line(line: &str) -> serde_json::Result<CargoLine> {
    let mut deserializer = serde_json::Deserializer::from_str(line);
    // Rust macro backtraces can exceed JSON's default nesting limit. Grow
    // the stack while decoding rather than dropping those diagnostics.
    deserializer.disable_recursion_limit();
    let parsed = CargoLine::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(parsed)
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
    /// The earlier findings not yet matched, when the check has a baseline.
    baseline: Option<Baseline>,
    baselined: BTreeMap<String, usize>,
    warnings: Vec<Problem>,
    errors: Vec<Problem>,
}

impl Sorted {
    /// Whether the baseline has this finding, counting it as baselined if so.
    fn in_baseline(&mut self, rule: &str, path: &str, source: Option<&str>) -> bool {
        let matched = self
            .baseline
            .as_mut()
            .is_some_and(|baseline| baseline.take(rule, path, source));
        if matched {
            *self.baselined.entry(rule.to_owned()).or_insert(0) += 1;
        }
        matched
    }
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

/// Sorts one diagnostic into a finding, a skipped or baselined finding, a
/// build error, a configuration or unavailable-lint warning, or noise,
/// echoing diagnostics when `echo` is set. Without a primary span, only unknown or removed lint
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
    // Recursive expansions can point into a dependency. Use the invocation
    // for the location, deduplication, skips and source-based baseline.
    let span = span.call_site();
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
    let source = span.text.first().map(|line| line.text.trim().to_owned());
    if let Some(rule) = &code
        && setting.rules.contains(rule)
    {
        let path = relative(setting.root, &file);
        if setting.config.skips(rule, path) {
            *sorted.skipped.entry(rule.clone()).or_insert(0) += 1;
            return;
        }
        if sorted.in_baseline(rule, &portable_path(setting.root, &file), source.as_deref()) {
            return;
        }
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
            source,
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

/// `file` relative to `root`, with `/` between segments, for matching a
/// baseline. A report names files with the separators of the platform that
/// wrote it, so a baseline written on Windows names `src\lib.rs` what Linux
/// names `src/lib.rs`, and its root is not a path Linux can strip.
fn portable_path(root: &Path, file: &str) -> String {
    let root = root.to_string_lossy().replace('\\', "/");
    let file = file.replace('\\', "/");
    file.strip_prefix(root.trim_end_matches('/'))
        .and_then(|rest| rest.strip_prefix('/'))
        .map_or_else(|| file.clone(), str::to_owned)
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
                expansion: None,
            }],
        }
    }

    fn classify_diagnostics(diagnostics: Vec<Diagnostic>) -> Report {
        classify_against(None, diagnostics)
    }

    fn classify_against(baseline: Option<Baseline>, diagnostics: Vec<Diagnostic>) -> Report {
        let config = Config::default();
        classify_with_config(&config, baseline, diagnostics)
    }

    fn classify_with_config(
        config: &Config,
        baseline: Option<Baseline>,
        diagnostics: Vec<Diagnostic>,
    ) -> Report {
        let root = Path::new("project");
        let rules = lints::selected(false).map(Lint::qualified).collect();
        let setting = Setting {
            root,
            clippy_config: Path::new("generated/clippy.toml"),
            config,
            rules: &rules,
            echo: false,
        };
        let mut sorted = Sorted {
            baseline,
            ..Sorted::default()
        };
        for diagnostic in diagnostics {
            classify(diagnostic, &setting, &mut sorted);
        }
        Report {
            schema_version: SCHEMA_VERSION,
            root: root.to_path_buf(),
            status: Status::Clean,
            findings: sorted.findings,
            skipped: sorted.skipped,
            baselined: sorted.baselined,
            baseline_unmatched: sorted.baseline.as_ref().map(Baseline::unmatched),
            warnings: sorted.warnings,
            errors: sorted.errors,
        }
    }

    fn expanded_macro(file: &str, line: u64, column: u64, text: &str) -> Diagnostic {
        let mut span = serde_json::json!({
            "file_name": file,
            "line_start": line,
            "column_start": column,
            "is_primary": false,
            "text": [{ "text": text }],
            "expansion": null,
        });
        for line in [808, 780, 765] {
            span = serde_json::json!({
                "file_name": "/registry/macros/src/lib.rs",
                "line_start": line,
                "column_start": 9,
                "is_primary": true,
                "text": [{ "text": "$crate::recursive!(@next)" }],
                "expansion": {
                    "span": span,
                    "macro_decl_name": "recursive!",
                    "def_site_span": null,
                },
            });
        }
        serde_json::from_value(serde_json::json!({
            "message": "use of a disallowed method",
            "level": "warning",
            "code": { "code": "clippy::disallowed_methods" },
            "spans": [span],
            "rendered": null,
        }))
        .unwrap()
    }

    #[test]
    fn macro_findings_use_call_sites_and_deduplicate_expansions() {
        let source = "    recursive!();";
        let mut another_expansion = expanded_macro("src/lib.rs", 10, 5, source);
        another_expansion.spans.first_mut().unwrap().line_start = 774;
        let report = classify_diagnostics(vec![
            expanded_macro("src/lib.rs", 10, 5, source),
            expanded_macro("src/lib.rs", 10, 5, source),
            another_expansion,
            expanded_macro("src/lib.rs", 20, 5, source),
            expanded_macro("src/lib.rs", 20, 25, source),
            expanded_macro("src/other.rs", 10, 5, source),
        ]);
        let locations: Vec<_> = report
            .findings
            .iter()
            .map(|finding| (finding.file.as_str(), finding.line, finding.column))
            .collect();
        assert_eq!(
            locations,
            [
                ("src/lib.rs", Some(10), Some(5)),
                ("src/lib.rs", Some(20), Some(5)),
                ("src/lib.rs", Some(20), Some(25)),
                ("src/other.rs", Some(10), Some(5)),
            ]
        );
        for finding in report.findings {
            assert_eq!(finding.source.as_deref(), Some("recursive!();"));
        }
    }

    #[test]
    fn deep_macro_backtraces_are_not_dropped_by_the_json_parser() {
        let mut span = serde_json::json!({
            "file_name": "src/lib.rs",
            "line_start": 2,
            "column_start": 5,
            "is_primary": false,
            "text": [{ "text": "    deep!();" }],
            "expansion": null,
        });
        for line in 1..=70 {
            span = serde_json::json!({
                "file_name": "/registry/macros/src/lib.rs",
                "line_start": line,
                "column_start": 9,
                "is_primary": true,
                "text": [],
                "expansion": { "span": span },
            });
        }
        let line = serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "message": "use of a disallowed method",
                "level": "warning",
                "code": { "code": "clippy::disallowed_methods" },
                "spans": [span],
                "rendered": null,
            },
        })
        .to_string();
        let parsed = parse_cargo_line(&line).unwrap();
        let report = classify_diagnostics(vec![parsed.message.unwrap()]);
        assert_eq!(report.findings.len(), 1);
        let finding = report.findings.first().unwrap();
        assert_eq!(finding.file, "src/lib.rs");
        assert_eq!(finding.line, Some(2));
        assert_eq!(finding.column, Some(5));
        assert_eq!(finding.source.as_deref(), Some("deep!();"));
        assert!(parse_cargo_line(&format!("{line} trailing data")).is_err());
    }

    #[test]
    fn macro_baselines_match_caller_source_and_count_distinct_invocations() {
        let baseline = baseline(
            "/elsewhere/project",
            serde_json::json!([earlier(
                "clippy::disallowed_methods",
                "src/lib.rs",
                Some("recursive!();"),
            )]),
        );
        let report = classify_against(
            Some(baseline),
            vec![
                expanded_macro("project/src/lib.rs", 40, 9, "        recursive!();"),
                expanded_macro("project/src/lib.rs", 40, 9, "        recursive!();"),
                expanded_macro("project/src/lib.rs", 50, 9, "        recursive!();"),
                expanded_macro("src/lib.rs", 60, 5, "    recursive!(edited);"),
                expanded_macro("src/other.rs", 40, 5, "    recursive!();"),
            ],
        );
        let locations: Vec<_> = report
            .findings
            .iter()
            .map(|finding| (finding.file.as_str(), finding.line))
            .collect();
        assert_eq!(
            locations,
            [
                ("project/src/lib.rs", Some(50)),
                ("src/lib.rs", Some(60)),
                ("src/other.rs", Some(40)),
            ]
        );
        assert_eq!(
            report.baselined,
            BTreeMap::from([("clippy::disallowed_methods".to_owned(), 1)])
        );
        assert_eq!(report.baseline_unmatched, Some(0));
    }

    #[test]
    fn macro_skip_paths_and_exceptions_match_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package.metadata.adamantium]\nskip = [\"tests/\"]\n\n[package.metadata.adamantium.exceptions]\ndisallowed_methods = [\"src/allowed.rs\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        let report = classify_with_config(
            &config,
            None,
            vec![
                expanded_macro("project/tests/cases.rs", 10, 5, "    recursive!();"),
                expanded_macro("project/tests/cases.rs", 10, 5, "    recursive!();"),
                expanded_macro("src/allowed.rs", 10, 5, "    recursive!();"),
                expanded_macro("src/lib.rs", 10, 5, "    recursive!();"),
            ],
        );
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings.first().unwrap().file, "src/lib.rs");
        assert_eq!(
            report.skipped,
            BTreeMap::from([("clippy::disallowed_methods".to_owned(), 2)])
        );
    }

    #[test]
    fn recorded_tokio_expansion_matches_the_rendered_location() {
        // Clippy 1.100.0 with tokio 1.54.1 and tokio::select disallowed.
        let diagnostic: Diagnostic =
            serde_json::from_str(include_str!("../tests/diagnostics/tokio-select.json")).unwrap();
        let rendered = diagnostic.rendered.as_ref().unwrap().clone();
        let config = Config::default();
        let rules = HashSet::from(["clippy::disallowed_macros".to_owned()]);
        let setting = Setting {
            root: Path::new("project"),
            clippy_config: Path::new("generated/clippy.toml"),
            config: &config,
            rules: &rules,
            echo: false,
        };
        let mut sorted = Sorted::default();
        classify(diagnostic, &setting, &mut sorted);
        assert_eq!(sorted.findings.len(), 1);
        let finding = sorted.findings.first().unwrap();
        assert_eq!(finding.file, "src/lib.rs");
        assert_eq!(finding.line, Some(2));
        assert_eq!(finding.column, Some(5));
        assert_eq!(finding.source.as_deref(), Some("tokio::select! {"));
        assert!(rendered.contains("--> src/lib.rs:2:5"), "{rendered}");
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

    /// An unwrap finding in `file` at `line`, on a line reading `text`.
    fn unwrap_at(file: &str, line: u64, text: &str, message: &str) -> Diagnostic {
        let mut diagnostic = diagnostic(file, "warning", Some("clippy::unwrap_used"), message);
        let span = diagnostic.spans.first_mut().unwrap();
        span.line_start = line;
        span.text = vec![SpanLine {
            text: text.to_owned(),
        }];
        diagnostic
    }

    fn baseline(root: &str, findings: serde_json::Value) -> Baseline {
        Baseline::parse(
            &serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "root": root,
                "status": "findings",
                "findings": findings,
            })
            .to_string(),
        )
        .unwrap()
    }

    fn earlier(rule: &str, file: &str, source: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "rule": rule,
            "message": "an older wording",
            "file": file,
            "line": 1,
            "column": 1,
            "source": source,
        })
    }

    #[test]
    fn baseline_matches_by_rule_path_and_trimmed_line_text() {
        let baseline = baseline(
            "/elsewhere/project",
            serde_json::json!([
                earlier(
                    "clippy::unwrap_used",
                    "src/lib.rs",
                    Some("let a = x.unwrap();")
                ),
                earlier(
                    "clippy::unwrap_used",
                    "/elsewhere/project/src/main.rs",
                    Some("let b = y.unwrap();"),
                ),
                earlier(
                    "clippy::unwrap_used",
                    "src/gone.rs",
                    Some("let c = z.unwrap();")
                ),
            ]),
        );
        let report = classify_against(
            Some(baseline),
            vec![
                // Moved down and reindented, with Clippy's message reworded.
                unwrap_at(
                    "src/lib.rs",
                    40,
                    "        let a = x.unwrap();",
                    "new wording",
                ),
                // Named relative to this project's root, not the earlier one.
                unwrap_at("project/src/main.rs", 3, "let b = y.unwrap();", "unwrap"),
                // The line was edited, so the finding is new.
                unwrap_at("src/lib.rs", 41, "let a = x.unwrap(); // edited", "unwrap"),
                // The same line in another file is new.
                unwrap_at("src/other.rs", 1, "let a = x.unwrap();", "unwrap"),
            ],
        );
        let new: Vec<(&str, u64)> = report
            .findings
            .iter()
            .map(|finding| (finding.file.as_str(), finding.line.unwrap()))
            .collect();
        assert_eq!(new, [("src/lib.rs", 41), ("src/other.rs", 1)]);
        assert_eq!(
            report.baselined,
            BTreeMap::from([("clippy::unwrap_used".to_owned(), 2)])
        );
        assert_eq!(report.baseline_unmatched, Some(1));
    }

    #[test]
    fn baseline_counts_findings_that_share_a_line_text() {
        let line = Some("v.unwrap();");
        let baseline = baseline(
            "/project",
            serde_json::json!([
                earlier("clippy::unwrap_used", "src/lib.rs", line),
                earlier("clippy::unwrap_used", "src/lib.rs", line),
            ]),
        );
        let report = classify_against(
            Some(baseline),
            vec![
                unwrap_at("src/lib.rs", 1, "v.unwrap();", "unwrap"),
                unwrap_at("src/lib.rs", 2, "v.unwrap();", "unwrap"),
                unwrap_at("src/lib.rs", 3, "v.unwrap();", "unwrap"),
            ],
        );
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings.first().unwrap().line, Some(3));
        assert_eq!(report.baseline_unmatched, Some(0));
    }

    #[test]
    fn baseline_only_matches_rule_findings() {
        let baseline = baseline(
            "/project",
            serde_json::json!([earlier(
                "clippy::unwrap_used",
                "src/lib.rs",
                Some("let value = option.unwrap();")
            ),]),
        );
        let report = classify_against(
            Some(baseline),
            vec![
                diagnostic("src/lib.rs", "error", Some("E0001"), "compile error"),
                diagnostic(
                    "src/lib.rs",
                    "warning",
                    Some("clippy::unwrap_used"),
                    "unwrap",
                ),
            ],
        );
        assert!(report.findings.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.skipped, BTreeMap::new());
        assert_eq!(report.baseline_unmatched, Some(0));

        let report = classify_diagnostics(vec![diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::unwrap_used"),
            "unwrap",
        )]);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.baselined, BTreeMap::new());
        assert_eq!(report.baseline_unmatched, None);
    }

    #[test]
    fn baseline_rejects_reports_it_cannot_trust() {
        let report = |version: serde_json::Value, status: &str| {
            serde_json::json!({
                "schema_version": version,
                "root": "/project",
                "status": status,
                "findings": [],
            })
            .to_string()
        };
        for (text, expected) in [
            (report(2.into(), "findings"), "it has schema version 2"),
            (report(4.into(), "clean"), "it has schema version 4"),
            ("{}".to_owned(), "it has no schema version"),
            ("not json".to_owned(), "it is not a JSON report"),
            (
                serde_json::json!({ "schema_version": SCHEMA_VERSION }).to_string(),
                "it is not a check report",
            ),
            (
                report(SCHEMA_VERSION.into(), "error"),
                "the project did not build",
            ),
        ] {
            let error = Baseline::parse(&text).unwrap_err().to_string();
            assert!(error.starts_with(expected), "{text}: {error}");
        }
        assert_eq!(
            Baseline::parse(&report(SCHEMA_VERSION.into(), "clean")).unwrap(),
            Baseline::default()
        );
    }

    #[test]
    fn baseline_paths_match_across_platforms() {
        for (root, file) in [
            (r"C:\work\project", r"src\lib.rs"),
            (r"C:\work\project", r"C:\work\project\src\lib.rs"),
            (r"C:\work\project\", r"C:\work\project\src\lib.rs"),
            ("/home/ci/project", "/home/ci/project/src/lib.rs"),
            ("/home/ci/project/", "src/lib.rs"),
        ] {
            assert_eq!(
                portable_path(Path::new(root), file),
                "src/lib.rs",
                "{root} {file}"
            );
        }
        // A sibling directory that shares the root's prefix is not inside it.
        assert_eq!(
            portable_path(
                Path::new("/home/ci/project"),
                "/home/ci/project-two/src/lib.rs"
            ),
            "/home/ci/project-two/src/lib.rs"
        );

        let baseline = baseline(
            r"C:\work\project",
            serde_json::json!([
                earlier(
                    "clippy::unwrap_used",
                    r"src\lib.rs",
                    Some("let a = x.unwrap();")
                ),
                earlier(
                    "clippy::unwrap_used",
                    r"C:\work\project\src\main.rs",
                    Some("let b = y.unwrap();"),
                ),
            ]),
        );
        let report = classify_against(
            Some(baseline),
            vec![
                unwrap_at("src/lib.rs", 1, "let a = x.unwrap();", "unwrap"),
                unwrap_at("project/src/main.rs", 1, "let b = y.unwrap();", "unwrap"),
            ],
        );
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.baseline_unmatched, Some(0));
    }
}
