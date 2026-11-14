//! Conservative fixes: compiler suggestions first, then guide-approved syntax.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use proc_macro2::Span;
use serde_json::Value;
use syn::{spanned::Spanned, visit::Visit};

use crate::{check, manifest};

mod scan;
mod state;

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
    // A shared-state rewrite changes every use of a binding at once, so it
    // plans a whole file and its edits stand or fall together. A field's
    // rewrite also depends on the rest of its package.
    let mut packages: BTreeMap<PathBuf, Vec<state::Draft>> = BTreeMap::new();
    for (file, mutexes) in mutexes(&diagnostics, &root) {
        if !migration_available(&file, &root, Api::Core)? {
            continue;
        }
        let Some(package) = package(&file, &root) else {
            continue;
        };
        let source = fs::read_to_string(&file)?;
        packages
            .entry(package)
            .or_default()
            .push(state::plan(&file, &source, &mutexes));
    }
    for (package, drafts) in packages {
        let krate = scan::Crate::scan(&package)?;
        for (file, outcome) in state::settle(drafts, &krate) {
            for refusal in &outcome.refusals {
                eprintln!(
                    "{}:{}: `{}` stays an `Arc<Mutex<_>>`: {}\n  Review {GUIDE}#{}",
                    file.strip_prefix(&root).unwrap_or(&file).display(),
                    refusal.line,
                    refusal.name,
                    refusal.reason.describe(),
                    refusal.reason.section()
                );
            }
            if !accept(&mut edits, outcome.edits) {
                eprintln!(
                    "adamantium: {}: shared-state rewrites overlap another fix; run fix again once it applies",
                    file.display()
                );
            }
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

/// Adds a suggestion's edits unless any of them conflicts, returning whether
/// it was added.
fn accept(edits: &mut Vec<Edit>, mut suggestion: Vec<Edit>) -> bool {
    suggestion.sort();
    suggestion.dedup();
    // Multipart suggestions are atomic, including conflicts within a group.
    let internally_conflicting = suggestion.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| conflicts(a, b))
    });
    let accepted = !internally_conflicting
        && suggestion
            .iter()
            .all(|edit| !edits.iter().any(|other| conflicts(edit, other)));
    if accepted {
        edits.extend(suggestion);
    }
    accepted
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
    if !migration_available(&file, root, Api::Tokio)? {
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
        end: span["byte_end"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(start),
        message,
        edits: Vec::new(),
    };
    visitor.visit_file(&tree);
    Ok(visitor.edits.into_iter().map(|edit| vec![edit]).collect())
}

/// The byte ranges in each project file that Clippy resolved to
/// `std::sync::Mutex`, outside macro expansions.
fn mutexes(diagnostics: &[Value], root: &Path) -> BTreeMap<PathBuf, BTreeSet<(usize, usize)>> {
    let mut files: BTreeMap<PathBuf, BTreeSet<(usize, usize)>> = BTreeMap::new();
    for diagnostic in diagnostics {
        if diagnostic
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            != Some("clippy::disallowed_types")
            || !diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("`std::sync::Mutex`"))
        {
            continue;
        }
        for span in diagnostic["spans"].as_array().into_iter().flatten() {
            if span["is_primary"] != true || !span["expansion"].is_null() {
                continue;
            }
            let range = span["byte_start"]
                .as_u64()
                .zip(span["byte_end"].as_u64())
                .and_then(|(start, end)| {
                    Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
                });
            if let Some(file) = span["file_name"]
                .as_str()
                .and_then(|name| source_file(root, name))
                && let Some(range) = range
            {
                files.entry(file).or_default().insert(range);
            }
        }
    }
    files
}

/// How much of Adamantium a file's package can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Api {
    None,
    /// Values and actors, which need no runtime.
    Core,
    /// Everything, including tasks, time and blocking work.
    Tokio,
}

/// Whether `file` can use `needs`, saying why not if it can't.
fn migration_available(file: &Path, root: &Path, needs: Api) -> Result<bool> {
    let available = adamantium_api(file, root)? >= needs;
    if !available {
        eprintln!(
            "adamantium: {}: migration rewrites require a nonoptional, noninherited `adamantium` dependency{}",
            file.display(),
            if needs == Api::Tokio {
                " with default features or explicit `tokio` features"
            } else {
                ""
            }
        );
    }
    Ok(available)
}

/// The directory of the package whose manifest is nearest above `file`.
fn package(file: &Path, root: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .take_while(|path| path.starts_with(root))
        .find(|ancestor| ancestor.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

// A migration must not introduce an unresolved crate path. Aliased dependencies
// are left for review rather than guessing which name is available in scope.
fn adamantium_api(file: &Path, root: &Path) -> Result<Api> {
    let Some(package) = package(file, root) else {
        return Ok(Api::None);
    };
    let doc: toml_edit::DocumentMut = fs::read_to_string(package.join("Cargo.toml"))?.parse()?;
    let Some(dependency) = doc
        .get("dependencies")
        .and_then(|deps| deps.get("adamantium"))
    else {
        return Ok(Api::None);
    };
    // Optional and inherited dependencies need feature/activation
    // resolution for the selected build. Leave those for review.
    let flag = |name| dependency.get(name).and_then(toml_edit::Item::as_bool);
    let renamed = dependency.get("package").and_then(toml_edit::Item::as_str);
    let tokio = dependency
        .get("features")
        .and_then(toml_edit::Item::as_array)
        .is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature.as_str() == Some("tokio"))
        });
    if flag("optional") == Some(true)
        || flag("workspace") == Some(true)
        || renamed.is_some_and(|name| name != "adamantium")
    {
        return Ok(Api::None);
    }
    Ok(if flag("default-features") != Some(false) || tokio {
        Api::Tokio
    } else {
        Api::Core
    })
}

struct Mechanical<'a> {
    source: &'a str,
    prefix: usize,
    file: PathBuf,
    start: usize,
    end: usize,
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
        self.range(span)
            .is_some_and(|r| r.start == self.start && r.end == self.end)
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
    fn visit_expr_await(&mut self, node: &'ast syn::ExprAwait) {
        // Only a directly awaited call preserves Tokio's timer-construction
        // boundary. Exact path spans avoid discarding compound callee effects.
        if self.message.contains("`tokio::time::sleep`")
            && let syn::Expr::Call(call) = &*node.base
            && let syn::Expr::Path(path) = &*call.func
            && self.matches(path.span())
        {
            self.replace(path.span(), "::adamantium::time::sleep".to_owned());
        }
        syn::visit::visit_expr_await(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if self.message.contains("`tokio::task::spawn_blocking`")
            && node.method == "unwrap"
            && node.args.is_empty()
            && let syn::Expr::Await(awaited) = &*node.receiver
            && let syn::Expr::Call(call) = &*awaited.base
            && let syn::Expr::Path(path) = &*call.func
            && self.matches(path.span())
            // The wrapper has a different generic parameter order. Retaining
            // inference by dropping a turbofish can silently change the result.
            && path.path.segments.iter().all(|segment| matches!(segment.arguments, syn::PathArguments::None))
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
    } else if message.contains("across an await")
        || message.contains("`tokio::sync::Mutex`")
        || message.contains("`futures::lock::Mutex`")
    {
        "a-lock-held-across-an-await"
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
    fn locks_held_across_an_await_link_to_their_section() {
        for message in [
            "this `MutexGuard` is held across an await point",
            "use of a disallowed type `tokio::sync::Mutex`",
            "use of a disallowed type `futures::lock::Mutex`",
        ] {
            assert_eq!(section(message), "a-lock-held-across-an-await");
        }
        assert_eq!(
            section("use of a disallowed type `std::sync::Mutex`"),
            "shared-state"
        );
        assert_eq!(
            section("use of a disallowed type `tokio::sync::RwLock`"),
            "rwlock"
        );
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
