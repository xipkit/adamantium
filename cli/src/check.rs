use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::Config,
    lints::{self, EXPERIMENTAL_LINTS, OVERFLOW_RULE},
    manifest, paths, sarif,
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

#[derive(Debug)]
struct Baseline {
    original: String,
    report: Value,
    remaining: BTreeMap<BaselineKey, VecDeque<usize>>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BaselineKey {
    rule: String,
    path: String,
    source: Option<String>,
}

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
    fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(text).with_context(|| format!("using {} as a baseline", path.display()))
    }

    fn parse(original: String) -> Result<Self> {
        let report: Value = serde_json::from_str(&original).context("it is not a JSON report")?;
        let version = report.get("schema_version").and_then(Value::as_u64);
        if version != Some(u64::from(SCHEMA_VERSION)) {
            let found = version.map_or_else(
                || "no schema version".to_owned(),
                |version| format!("schema version {version}"),
            );
            bail!(
                "it has {found}, but a baseline must be a schema version {SCHEMA_VERSION} report; regenerate it with `cargo adamantium check --format json`"
            );
        }
        let earlier = EarlierReport::deserialize(&report).context("it is not a check report")?;
        if earlier.status == Status::Error {
            bail!("the project did not build when it was written, so its findings are incomplete");
        }
        let mut remaining = BTreeMap::<_, VecDeque<_>>::new();
        for (index, finding) in earlier.findings.into_iter().enumerate() {
            let key = BaselineKey {
                path: portable_path(&earlier.root, &finding.file),
                rule: finding.rule,
                source: finding.source,
            };
            remaining.entry(key).or_default().push_back(index);
        }
        Ok(Self {
            original,
            report,
            remaining,
        })
    }

    fn take(&mut self, rule: &str, path: &str, source: Option<&str>) -> bool {
        let key = BaselineKey {
            rule: rule.to_owned(),
            path: path.to_owned(),
            source: source.map(str::to_owned),
        };
        self.remaining
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
            .is_some()
    }

    fn unmatched(&self) -> usize {
        self.remaining.values().map(VecDeque::len).sum()
    }

    fn prune(mut self, path: &Path) -> Result<usize> {
        let stale: HashSet<usize> = self.remaining.values().flatten().copied().collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let findings = self
            .report
            .get_mut("findings")
            .and_then(Value::as_array_mut)
            .context("the baseline report has no findings array")?;
        let mut index = 0;
        findings.retain(|_| {
            let matched = !stale.contains(&index);
            index += 1;
            matched
        });
        if findings.is_empty() {
            *self
                .report
                .get_mut("status")
                .context("the baseline report has no status")? =
                serde_json::to_value(Status::Clean)?;
        }
        replace_baseline(path, &self.original, &self.report)
            .with_context(|| format!("updating baseline {}", path.display()))?;
        Ok(stale.len())
    }
}

fn replace_baseline(path: &Path, original: &str, report: &Value) -> Result<()> {
    let path = path.canonicalize()?;
    let parent = path
        .parent()
        .context("the baseline file has no parent directory")?;
    let mut updated = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut updated, report)?;
    writeln!(updated)?;

    #[cfg(unix)]
    let _lock = {
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        file.lock()?;
        file
    };
    updated
        .as_file()
        .set_permissions(fs::metadata(&path)?.permissions())?;
    updated.as_file().sync_all()?;
    if fs::read_to_string(&path)? != original {
        bail!("the baseline changed during the check; rerun the check to update it");
    }
    updated.persist(&path)?;
    Ok(())
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

fn findings_label(count: usize) -> &'static str {
    if count == 1 { "finding" } else { "findings" }
}

fn print_text(stderr: &mut impl Write, report: &Report, summary: bool) -> io::Result<()> {
    for error in &report.errors {
        if error.file.is_none() {
            writeln!(stderr, "error: {}", error.message)?;
        }
    }
    let note = left_out_note(report);
    let count = report.findings.len();
    let label = findings_label(count);
    match report.status {
        Status::Clean => writeln!(stderr, "adamantium: clean{note}"),
        Status::Findings if summary => {
            writeln!(stderr, "adamantium: {count} {label}{note}")?;
            writeln!(stderr, "Findings by rule:")?;
            for (rule, count) in
                ranked_counts(report.findings.iter().map(|finding| finding.rule.as_str()))
            {
                writeln!(stderr, "  {count:>5}  {rule}")?;
            }
            writeln!(stderr, "Top hotspot files:")?;
            let files = ranked_counts(report.findings.iter().map(|finding| finding.file.as_str()));
            for (file, count) in files.into_iter().take(10) {
                writeln!(stderr, "  {count:>5}  {file}")?;
            }
            Ok(())
        }
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
                "adamantium: {count} {label} in {}{note}",
                files.join(", ")
            )
        }
        Status::Error => writeln!(
            stderr,
            "adamantium: the project did not build, so the rules were not all checked"
        ),
    }
}

fn ranked_counts<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(&'a str, usize)> {
    let mut counts = BTreeMap::new();
    for name in names {
        *counts.entry(name).or_insert(0) += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|&(name, count)| (Reverse(count), name));
    counts
}

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

fn write_clippy_config(
    dir: &Path,
    strict: bool,
    fix: bool,
) -> Result<(PathBuf, std::borrow::Cow<'static, str>)> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let path = dir.join("clippy.toml");
    let mut toml = lints::clippy_toml(strict)?;
    if fix {
        toml = lints::fix_clippy_toml(&toml)?.into();
    }
    fs::write(&path, toml.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, toml))
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

    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root)
        .env("CLIPPY_CONF_DIR", conf.path())
        .args(["clippy", "--message-format=json"]);
    if !selects_targets(cargo_args) {
        command.arg("--all-targets");
    }
    if !selects_packages(cargo_args) {
        command.arg("--workspace");
    }
    command
        .args(cargo_args)
        .args(["--", "--cap-lints", "warn", "-A", "clippy::all"]);
    for lint in lints::selected(strict) {
        command.args(["-W", &lint.qualified()]);
    }
    let mut commands = vec![command];
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
            Err(error) => sorted.errors.push(Problem {
                message: format!("experimental check incomplete: {error:#}"),
                file: None,
                line: None,
            }),
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
    for mut command in commands {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                sorted.errors.push(Problem {
                    message: format!("check incomplete: {error}"),
                    file: None,
                    line: None,
                });
                complete = false;
                continue;
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
                            classify(diagnostic, &setting, &mut sorted);
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
        if !built || !exit.success() {
            complete = false;
            sorted.errors.push(Problem {
                message: format!(
                    "check incomplete: compiler or lint driver did not finish successfully ({exit})"
                ),
                file: None,
                line: None,
            });
        }
    }

    let overflow_unchecked = !manifest::release_checks_overflow(root)?;
    if overflow_unchecked && config.skips(OVERFLOW_RULE, "Cargo.toml") {
        *sorted.skipped.entry(OVERFLOW_RULE.to_owned()).or_insert(0) += 1;
    } else if overflow_unchecked && !sorted.in_baseline(OVERFLOW_RULE, "Cargo.toml", None) {
        let message = "[profile.release] does not set overflow-checks = true, so integer overflow wraps silently in release builds; `cargo adamantium init` adds it".to_owned();
        let remediation = lints::remediation(OVERFLOW_RULE, &message, &reasons);
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

fn parse_cargo_line(line: &str) -> serde_json::Result<CargoLine> {
    let mut deserializer = serde_json::Deserializer::from_str(line);
    deserializer.disable_recursion_limit();
    let parsed = CargoLine::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(parsed)
}

fn dylint_command(
    root: &Path,
    cargo_args: &[String],
    library: &Path,
    runner: Option<&Path>,
) -> Result<Command> {
    let library = library
        .canonicalize()
        .context("resolving the Dylint library; build lints/ first")?;
    anyhow::ensure!(library.is_file(), "Dylint library is not a file");
    let program = runner
        .map(Path::as_os_str)
        .map(ToOwned::to_owned)
        .or_else(|| env::var_os("CARGO"))
        .unwrap_or_else(|| "cargo".into());
    let listed = Command::new(&program)
        .current_dir(root)
        .args(["dylint", "list", "--no-metadata", "--lib-path"])
        .arg(&library)
        .stdin(Stdio::null())
        .output()
        .context("loading the experimental lints; install cargo-dylint 6.4.0 first")?;
    anyhow::ensure!(
        listed.status.success(),
        "loading the experimental lints: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listing = String::from_utf8_lossy(&listed.stdout);
    for lint in EXPERIMENTAL_LINTS {
        anyhow::ensure!(
            listing.split_whitespace().any(|word| word == *lint),
            "Dylint library does not register {lint}"
        );
    }
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .args(["dylint", "--no-metadata", "--lib-path"])
        .arg(library)
        .args(["--", "--message-format=json"]);
    if !selects_targets(cargo_args) {
        command.arg("--all-targets");
    }
    if !selects_packages(cargo_args) {
        command.arg("--workspace");
    }
    command.args(cargo_args);
    let mut flags = String::from("--cap-lints warn");
    for lint in EXPERIMENTAL_LINTS {
        flags.push_str(" -W ");
        flags.push_str(lint);
    }
    command.env("DYLINT_RUSTFLAGS", flags);
    Ok(command)
}

struct Setting<'a> {
    root: &'a Path,
    clippy_config: &'a Path,
    reasons: &'a lints::Reasons,
    config: &'a Config,
    rules: &'a HashSet<String>,
    echo: bool,
}

#[derive(Default)]
struct Sorted<'a> {
    seen: HashSet<DiagnosticKey>,
    findings: Vec<Finding>,
    skipped: BTreeMap<String, usize>,
    baseline: Option<&'a mut Baseline>,
    baselined: BTreeMap<String, usize>,
    warnings: Vec<Problem>,
    errors: Vec<Problem>,
}

impl Sorted<'_> {
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

fn selects_packages(cargo_args: &[String]) -> bool {
    cargo_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-p" | "--package" | "--workspace" | "--all" | "--exclude"
        ) || arg.starts_with("--package=")
            || (arg.starts_with("-p") && arg.len() > 2)
    })
}

fn selects_targets(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--all-targets"
                | "--lib"
                | "--bin"
                | "--bins"
                | "--example"
                | "--examples"
                | "--test"
                | "--tests"
                | "--bench"
                | "--benches"
        ) || ["--bin=", "--example=", "--test=", "--bench="]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
    })
}

fn classify(diagnostic: Diagnostic, setting: &Setting<'_>, sorted: &mut Sorted<'_>) {
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
        Some(rule) if setting.rules.contains(&rule) => {
            let remediation = lints::remediation(&rule, &diagnostic.message, setting.reasons);
            sorted.findings.push(Finding {
                rule,
                message: diagnostic.message,
                file,
                line: Some(span.line_start),
                column: Some(span.column_start),
                source,
                remediation,
            });
        }
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

fn relative<'a>(root: &Path, file: &'a str) -> &'a str {
    Path::new(file)
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or(file)
}

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
    paths::lexical(Path::new(path))
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
        mut baseline: Option<Baseline>,
        diagnostics: Vec<Diagnostic>,
    ) -> Report {
        let root = Path::new("project");
        let rules = lints::selected(false).map(Lint::qualified).collect();
        let setting = Setting {
            root,
            clippy_config: Path::new("generated/clippy.toml"),
            reasons: &lints::Reasons::parse(lints::CLIPPY_TOML),
            config,
            rules: &rules,
            echo: false,
        };
        let mut sorted = Sorted {
            baseline: baseline.as_mut(),
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
            baseline_unmatched: sorted.baseline.as_deref().map(Baseline::unmatched),
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
        let diagnostic: Diagnostic =
            serde_json::from_str(include_str!("../tests/diagnostics/tokio-select.json")).unwrap();
        let rendered = diagnostic.rendered.as_ref().unwrap().clone();
        let config = Config::default();
        let rules = HashSet::from(["clippy::disallowed_macros".to_owned()]);
        let setting = Setting {
            root: Path::new("project"),
            clippy_config: Path::new("generated/clippy.toml"),
            reasons: &lints::Reasons::parse(lints::CLIPPY_TOML),
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
        let (path, toml) = write_clippy_config(&link, false, false).unwrap();
        assert_eq!(
            path,
            real.path().canonicalize().unwrap().join("clippy.toml")
        );
        assert_eq!(toml, lints::CLIPPY_TOML);
        assert_eq!(fs::read_to_string(&path).unwrap(), lints::CLIPPY_TOML);
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
            serde_json::json!({
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
                unwrap_at(
                    "src/lib.rs",
                    40,
                    "        let a = x.unwrap();",
                    "new wording",
                ),
                unwrap_at("project/src/main.rs", 3, "let b = y.unwrap();", "unwrap"),
                unwrap_at("src/lib.rs", 41, "let a = x.unwrap(); // edited", "unwrap"),
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
            let error = Baseline::parse(text.clone()).unwrap_err().to_string();
            assert!(error.starts_with(expected), "{text}: {error}");
        }
        assert_eq!(
            Baseline::parse(report(SCHEMA_VERSION.into(), "clean"))
                .unwrap()
                .unmatched(),
            0
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

    #[test]
    fn findings_carry_rule_and_path_specific_remediation() {
        let report = classify_diagnostics(vec![
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::disallowed_methods"),
                "use of a disallowed method `tokio::spawn`",
            ),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::disallowed_methods"),
                "use of a disallowed method `std::thread::Builder::spawn`",
            ),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::disallowed_methods"),
                "use of a disallowed method `std::mem::forget`",
            ),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::disallowed_types"),
                "use of a disallowed type `std::sync::Mutex`",
            ),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::disallowed_types"),
                "use of a disallowed type `tokio::sync::RwLock`",
            ),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::unwrap_used"),
                "used `unwrap()` on an `Option` value",
            ),
        ]);
        let remediations: Vec<Option<&str>> = report
            .findings
            .iter()
            .map(|finding| finding.remediation.as_deref())
            .collect();
        assert_eq!(
            remediations,
            [
                Some(
                    "An unstructured task; spawn inside adamantium::Tasks. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#tasks."
                ),
                Some(
                    "An unstructured thread; use adamantium::Tasks or adamantium::blocking. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#blocking-work."
                ),
                Some("Leaks. Run cargo adamantium explain clippy::disallowed_methods."),
                Some(
                    "Use adamantium::Actor, which never holds its lock across an await or inside another actor. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#shared-state."
                ),
                Some(
                    "An async lock can be held across an await; use adamantium::Actor. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#rwlock."
                ),
                Some(
                    "Return or handle None/Err. Propagate Result with ? so an enclosing adamantium::Tasks scope can observe failure. Run cargo adamantium explain clippy::unwrap_used."
                ),
            ]
        );

        let mut without_guidance = report.findings.first().unwrap().clone();
        without_guidance.remediation = None;
        let encoded = serde_json::to_value(&without_guidance).unwrap();
        assert!(encoded.get("remediation").is_none(), "{encoded}");
    }
}
