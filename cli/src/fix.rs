//! Conservative fixes: compiler suggestions first, then guide-approved syntax.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use proc_macro2::Span;
use serde_json::Value;
use syn::{spanned::Spanned, visit::Visit};

use crate::{check, manifest};

const GUIDE: &str = "https://github.com/xipkit/adamantium/blob/main/docs/migrating.md";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edit {
    file: PathBuf,
    start: usize,
    end: usize,
    replacement: String,
}

pub fn run(path: &Path, args: &[String], strict: bool, dry_run: bool) -> Result<ExitCode> {
    let root = manifest::root(path)?.canonicalize()?;
    let mut diagnostics = Vec::new();
    let report =
        check::check_with_suggestions(&root, args, false, strict, None, Some(&mut diagnostics))?;
    if report.status == check::Status::Error {
        for error in report.errors {
            eprintln!("{}", error.message);
        }
        bail!("project did not build; no fixes applied");
    }
    let mut edits = Vec::new();
    let mut fallback = Vec::new();
    for diagnostic in &diagnostics {
        let mut suggestions = Vec::new();
        machine_suggestions(diagnostic, &root, &mut suggestions);
        if suggestions.is_empty() {
            fallback.push(diagnostic);
        }
        for suggestion in suggestions {
            accept(&mut edits, suggestion);
        }
    }
    // Every compiler suggestion gets priority, even when another diagnostic
    // appeared first in Cargo's stream.
    for diagnostic in fallback {
        for suggestion in mechanical(diagnostic, &root)? {
            accept(&mut edits, suggestion);
        }
    }
    edits.sort();
    edits.dedup();
    let mut files: BTreeMap<PathBuf, Vec<Edit>> = BTreeMap::new();
    for edit in edits {
        files.entry(edit.file.clone()).or_default().push(edit);
    }
    let mut changed = Vec::new();
    for (file, edits) in files {
        let before = fs::read_to_string(&file)?;
        let mut after = before.clone();
        for edit in edits.iter().rev() {
            if before.get(edit.start..edit.end).is_none() {
                bail!("invalid source span in {}", file.display());
            }
            after.replace_range(edit.start..edit.end, &edit.replacement);
        }
        if before != after {
            changed.push((file, before, after));
        }
    }
    // Read and validate every file before writing any file.
    for (file, before, after) in &changed {
        if dry_run {
            diff(file.strip_prefix(&root).unwrap_or(file), before, after);
        } else {
            if fs::read_to_string(file)? != *before {
                bail!("{} changed during fix", file.display());
            }
            fs::write(file, after).with_context(|| format!("writing {}", file.display()))?;
            eprintln!("adamantium: fixed {}", file.display());
        }
    }
    let remaining = if dry_run {
        report
    } else {
        check::check(&root, args, false, strict, None)?
    };
    for finding in &remaining.findings {
        eprintln!(
            "{}:{}: {}: {}\n  Review {GUIDE}#{}",
            finding.file,
            finding.line.unwrap_or(1),
            finding.rule,
            finding.message,
            section(&finding.message)
        );
    }
    for warning in &remaining.warnings {
        eprintln!("warning: {}", warning.message);
    }
    for error in &remaining.errors {
        eprintln!("{}", error.message);
    }
    eprintln!(
        "adamantium: {} file(s) {}; {} finding(s) remaining{}",
        changed.len(),
        if dry_run { "would change" } else { "changed" },
        remaining.findings.len(),
        if dry_run { " (before fixes)" } else { "" }
    );
    Ok(match remaining.status {
        check::Status::Clean => ExitCode::SUCCESS,
        check::Status::Findings => ExitCode::from(1),
        check::Status::Error => ExitCode::from(2),
    })
}

fn accept(edits: &mut Vec<Edit>, mut suggestion: Vec<Edit>) {
    suggestion.sort();
    suggestion.dedup();
    // Multipart suggestions are atomic, including conflicts within a group.
    let internally_conflicting = suggestion.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| conflicts(a, b))
    });
    if !internally_conflicting
        && suggestion
            .iter()
            .all(|edit| !edits.iter().any(|other| conflicts(edit, other)))
    {
        edits.extend(suggestion);
    }
}

fn conflicts(a: &Edit, b: &Edit) -> bool {
    a.file == b.file && a.start <= b.end && b.start <= a.end
}

fn source_file(root: &Path, name: &str) -> Option<PathBuf> {
    let file = root.join(name).canonicalize().ok()?;
    // Never modify dependencies or a symlink pointing out of the project.
    (file.starts_with(root) && file.extension().is_some_and(|ext| ext == "rs")).then_some(file)
}

fn machine_suggestions(node: &Value, root: &Path, output: &mut Vec<Vec<Edit>>) {
    if let Some(spans) = node["spans"].as_array() {
        let replacements: Vec<_> = spans
            .iter()
            .filter(|s| s["suggested_replacement"].is_string())
            .collect();
        if !replacements.is_empty() {
            let edits: Option<Vec<_>> = replacements
                .into_iter()
                .map(|s| {
                    if s["suggestion_applicability"] != "MachineApplicable"
                        || !s["expansion"].is_null()
                    {
                        return None;
                    }
                    Some(Edit {
                        file: source_file(root, s["file_name"].as_str()?)?,
                        start: usize::try_from(s["byte_start"].as_u64()?).ok()?,
                        end: usize::try_from(s["byte_end"].as_u64()?).ok()?,
                        replacement: s["suggested_replacement"].as_str()?.to_owned(),
                    })
                })
                .collect();
            if let Some(edits) = edits {
                output.push(edits);
            }
        }
    }
    if let Some(children) = node["children"].as_array() {
        for child in children {
            machine_suggestions(child, root, output);
        }
    }
}

fn mechanical(diagnostic: &Value, root: &Path) -> Result<Vec<Vec<Edit>>> {
    let message = diagnostic["message"].as_str().unwrap_or("");
    if diagnostic
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(Value::as_str)
        != Some("clippy::disallowed_methods")
        || !(message.contains("`tokio::time::sleep`")
            || message.contains("`tokio::task::spawn_blocking`"))
    {
        return Ok(Vec::new());
    }
    let Some(span) = diagnostic["spans"]
        .as_array()
        .and_then(|spans| spans.iter().find(|s| s["is_primary"] == true))
    else {
        return Ok(Vec::new());
    };
    if !span["expansion"].is_null() {
        return Ok(Vec::new());
    }
    let Some(file) = span["file_name"]
        .as_str()
        .and_then(|name| source_file(root, name))
    else {
        return Ok(Vec::new());
    };
    if !has_adamantium(&file, root)? {
        eprintln!(
            "adamantium: {}: add an `adamantium` dependency before applying migration rewrites",
            file.display()
        );
        return Ok(Vec::new());
    }
    let source = fs::read_to_string(&file)?;
    let Ok(tree) = syn::parse_file(&source) else {
        return Ok(Vec::new());
    };
    let Some(start) = span["byte_start"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
    else {
        return Ok(Vec::new());
    };
    let mut visitor = Mechanical {
        source: &source,
        prefix: usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
            + tree.shebang.as_ref().map_or(0, String::len),
        file,
        start,
        message,
        edits: Vec::new(),
    };
    visitor.visit_file(&tree);
    Ok(visitor.edits.into_iter().map(|edit| vec![edit]).collect())
}

// A migration must not introduce an unresolved crate path. Aliased dependencies
// are left for review rather than guessing which name is available in scope.
fn has_adamantium(file: &Path, root: &Path) -> Result<bool> {
    for ancestor in file
        .ancestors()
        .skip(1)
        .take_while(|path| path.starts_with(root))
    {
        let manifest = ancestor.join("Cargo.toml");
        if manifest.is_file() {
            let doc: toml_edit::DocumentMut = fs::read_to_string(manifest)?.parse()?;
            return Ok(doc
                .get("dependencies")
                .and_then(|deps| deps.get("adamantium"))
                .is_some());
        }
    }
    Ok(false)
}

struct Mechanical<'a> {
    source: &'a str,
    prefix: usize,
    file: PathBuf,
    start: usize,
    message: &'a str,
    edits: Vec<Edit>,
}

impl Mechanical<'_> {
    fn range(&self, span: Span) -> Option<std::ops::Range<usize>> {
        let range = span.byte_range();
        let range = (range.start + self.prefix)..(range.end + self.prefix);
        self.source.get(range.clone()).map(|_| range)
    }
    fn matches(&self, span: Span) -> bool {
        self.range(span).is_some_and(|r| r.contains(&self.start))
    }
    fn replace(&mut self, span: Span, replacement: String) {
        if let Some(range) = self.range(span) {
            self.edits.push(Edit {
                file: self.file.clone(),
                start: range.start,
                end: range.end,
                replacement,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Mechanical<'_> {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        // The diagnostic resolves aliases/imports; syntax alone cannot tell
        // whether a function called `sleep` belongs to Tokio.
        if self.message.contains("`tokio::time::sleep`") && self.matches(node.func.span()) {
            self.replace(node.func.span(), "::adamantium::time::sleep".to_owned());
        }
        syn::visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if self.message.contains("`tokio::task::spawn_blocking`")
            && node.method == "unwrap"
            && node.args.is_empty()
            && let syn::Expr::Await(awaited) = &*node.receiver
            && let syn::Expr::Call(call) = &*awaited.base
            && self.matches(call.func.span())
            && let Some(args) = self.range(call.paren_token.span.join())
            && let Some(text) = self.source.get(args)
        {
            self.replace(node.span(), format!("::adamantium::blocking{text}.await"));
            return;
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn section(message: &str) -> &'static str {
    if message.contains("spawn_blocking") || message.contains("thread::spawn") {
        "blocking-work"
    } else if message.contains("RwLock") {
        "rwlock"
    } else if message.contains("Mutex") || message.contains("lock") {
        "shared-state"
    } else if message.contains("time::") {
        "stopping-a-long-lived-task"
    } else {
        "tasks"
    }
}

// One whole-file unified hunk keeps the preview valid without an external diff
// executable, including files with no trailing newline.
fn diff(file: &Path, before: &str, after: &str) {
    println!("--- a/{}\n+++ b/{}", file.display(), file.display());
    println!(
        "@@ -1,{} +1,{} @@",
        before.lines().count(),
        after.lines().count()
    );
    for (prefix, text) in [('-', before), ('+', after)] {
        for line in text.split_inclusive('\n') {
            print!("{prefix}{line}");
            if !line.ends_with('\n') {
                println!("\n\\ No newline at end of file");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compiler_suggestions_keep_multipart_edits_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("lib.rs"), "let v = 1;\n").unwrap();
        let span = |start, end, replacement, applicability| {
            json!({
                "file_name": "lib.rs", "byte_start": start, "byte_end": end,
                "suggested_replacement": replacement,
                "suggestion_applicability": applicability, "expansion": null
            })
        };
        let mut output = Vec::new();
        machine_suggestions(
            &json!({"children": [{"spans": [
                span(0, 3, "const", "MachineApplicable"),
                span(5, 5, ": u32", "MachineApplicable")
            ]}]}),
            &root,
            &mut output,
        );
        assert_eq!(output.len(), 1);
        assert_eq!(output.first().unwrap().len(), 2);
        output.clear();
        machine_suggestions(
            &json!({"spans": [
                span(0, 3, "const", "MachineApplicable"),
                span(5, 5, ": u32", "MaybeIncorrect")
            ]}),
            &root,
            &mut output,
        );
        assert!(output.is_empty());
        let mut foreign = span(0, 3, "const", "MachineApplicable");
        foreign["file_name"] = json!(root.parent().unwrap().join("foreign.rs"));
        machine_suggestions(&json!({"spans": [foreign]}), &root, &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn conflicts_never_apply_part_of_a_suggestion() {
        let edit = |start, end| Edit {
            file: PathBuf::from("lib.rs"),
            start,
            end,
            replacement: String::new(),
        };
        let mut edits = vec![edit(2, 5)];
        accept(&mut edits, vec![edit(3, 4), edit(10, 12)]);
        assert_eq!(edits, vec![edit(2, 5)]);
        accept(&mut edits, vec![edit(10, 15), edit(12, 16)]);
        assert_eq!(edits, vec![edit(2, 5)]);
        accept(&mut edits, vec![edit(10, 12), edit(20, 22)]);
        assert_eq!(edits.len(), 3);
    }

    #[test]
    fn suggestions_in_macro_expansions_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "macro_rules! example {}\n").unwrap();
        let mut output = Vec::new();
        machine_suggestions(
            &json!({"spans": [{
                "file_name": "lib.rs", "byte_start": 0, "byte_end": 3,
                "suggested_replacement": "", "suggestion_applicability": "MachineApplicable",
                "expansion": {"span": {}}
            }]}),
            &dir.path().canonicalize().unwrap(),
            &mut output,
        );
        assert!(output.is_empty());
    }
}
