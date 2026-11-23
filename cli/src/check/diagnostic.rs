use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use serde::Deserialize;

use super::{
    Finding, Problem,
    baseline::{Baseline, portable_path},
};
use crate::{config::Config, lints, paths};

#[derive(Deserialize)]
pub(super) struct CargoLine {
    pub(super) reason: String,
    pub(super) message: Option<Diagnostic>,
    pub(super) success: Option<bool>,
}

#[derive(Deserialize)]
pub(super) struct Diagnostic {
    pub(super) message: String,
    pub(super) level: String,
    pub(super) code: Option<Code>,
    pub(super) rendered: Option<String>,
    #[serde(default)]
    pub(super) spans: Vec<Span>,
}

#[derive(Deserialize)]
pub(super) struct Code {
    pub(super) code: String,
}

#[derive(Deserialize)]
pub(super) struct Span {
    pub(super) file_name: String,
    pub(super) line_start: u64,
    pub(super) column_start: u64,
    pub(super) is_primary: bool,
    #[serde(default)]
    pub(super) text: Vec<SpanLine>,
    pub(super) expansion: Option<Box<Expansion>>,
}

#[derive(Deserialize)]
pub(super) struct Expansion {
    pub(super) span: Span,
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
pub(super) struct SpanLine {
    pub(super) text: String,
}

#[derive(PartialEq, Eq, Hash)]
pub(super) enum DiagnosticKey {
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

pub(super) fn parse_cargo_line(line: &str) -> serde_json::Result<CargoLine> {
    let mut deserializer = serde_json::Deserializer::from_str(line);
    deserializer.disable_recursion_limit();
    let parsed = CargoLine::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(parsed)
}

pub(super) struct Setting<'a> {
    pub(super) root: &'a Path,
    pub(super) clippy_config: &'a Path,
    pub(super) reasons: &'a lints::Reasons,
    pub(super) config: &'a Config,
    pub(super) rules: &'a HashSet<String>,
    pub(super) echo: bool,
}

#[derive(Default)]
pub(super) struct Sorted<'a> {
    pub(super) seen: HashSet<DiagnosticKey>,
    pub(super) findings: Vec<Finding>,
    pub(super) skipped: BTreeMap<String, usize>,
    pub(super) baseline: Option<&'a mut Baseline>,
    pub(super) baselined: BTreeMap<String, usize>,
    pub(super) warnings: Vec<Problem>,
    pub(super) errors: Vec<Problem>,
}

impl Sorted<'_> {
    pub(super) fn in_baseline(&mut self, rule: &str, path: &str, source: Option<&str>) -> bool {
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

pub(super) fn classify(diagnostic: Diagnostic, setting: &Setting<'_>, sorted: &mut Sorted<'_>) {
    let Some(span) = diagnostic.spans.iter().find(|span| span.is_primary) else {
        classify_spanless(diagnostic, setting, sorted);
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

/// A diagnostic without a primary span is kept only when it warns about an
/// unknown, renamed or removed lint, once for each code and message.
fn classify_spanless(diagnostic: Diagnostic, setting: &Setting<'_>, sorted: &mut Sorted<'_>) {
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
    sorted.warnings.push(Problem::unlocated(diagnostic.message));
}

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
    paths::lexical(Path::new(path))
        .to_string_lossy()
        .into_owned()
}
