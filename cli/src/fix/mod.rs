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

use crate::{
    check,
    lints::{GUIDE, Hint},
    manifest,
};

mod channels;
mod scan;
mod source;
mod state;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edit {
    file: PathBuf,
    start: usize,
    end: usize,
    replacement: String,
}

struct StateSource {
    file: PathBuf,
    source: String,
    mutexes: BTreeSet<(usize, usize)>,
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
    // The imports that each file's accepted rewrites can leave unused.
    let mut imports: BTreeMap<PathBuf, Vec<&[&str]>> = BTreeMap::new();
    // A shared-state rewrite changes every use of a binding at once, so it
    // plans a whole file and its edits stand or fall together. A field's
    // rewrite also depends on the rest of its package.
    let mut packages: BTreeMap<PathBuf, Vec<StateSource>> = BTreeMap::new();
    for (file, mutexes) in mutexes(&diagnostics, &root) {
        if !migration_available(&file, &root, Api::Core)? {
            continue;
        }
        let Some(package) = package(&file, &root) else {
            continue;
        };
        let source = fs::read_to_string(&file)?;
        packages.entry(package).or_default().push(StateSource {
            file,
            source,
            mutexes,
        });
    }
    for (package, drafts) in packages {
        let krate = scan::Crate::scan(&package)?;
        let drafts = drafts
            .into_iter()
            .map(|input| state::plan(&input.file, &input.source, &input.mutexes, &krate))
            .collect();
        for (file, outcome) in state::settle(drafts, &krate) {
            for refusal in &outcome.refusals {
                eprintln!(
                    "{}:{}: `{}` stays an `Arc<Mutex<_>>`: {}\n  Review {GUIDE}#{}",
                    file.strip_prefix(&root).unwrap_or(&file).display(),
                    refusal.line,
                    refusal.name,
                    refusal
                        .boundary
                        .as_deref()
                        .unwrap_or(refusal.reason.describe()),
                    refusal.reason.section()
                );
            }
            if outcome.edits.is_empty() {
                continue;
            }
            if accept(&mut edits, outcome.edits) {
                imports
                    .entry(file)
                    .or_default()
                    .extend_from_slice(state::IMPORTS);
            } else {
                eprintln!(
                    "adamantium: {}: shared-state rewrites overlap another fix; run fix again once it applies",
                    file.display()
                );
            }
        }
    }
    // A channel's rewrite changes its constructor and every use of its
    // halves, so the edits for a file stand or fall together.
    for (file, constructors) in channels(&diagnostics, &root) {
        if !migration_available(&file, &root, Api::Core)? {
            continue;
        }
        let source = fs::read_to_string(&file)?;
        let outcome = channels::plan(&file, &source, &constructors);
        for refusal in &outcome.refusals {
            eprintln!(
                "{}:{}: `{}` stays {}: {}\n  Review {GUIDE}#{}",
                file.strip_prefix(&root).unwrap_or(&file).display(),
                refusal.line,
                refusal.name,
                refusal.kind.describe(),
                refusal.reason,
                refusal.kind.section()
            );
        }
        if outcome.edits.is_empty() {
            continue;
        }
        if accept(&mut edits, outcome.edits) {
            imports
                .entry(file)
                .or_default()
                .extend_from_slice(channels::IMPORTS);
        } else {
            eprintln!(
                "adamantium: {}: channel rewrites overlap another fix; run fix again once it applies",
                file.display()
            );
        }
    }
    // One pass per file sees every rewrite at once, so the imports they leave
    // unused go together, whichever rewrite left each one.
    for (file, paths) in imports {
        let rewrites: Vec<Edit> = edits
            .iter()
            .filter(|edit| edit.file == file)
            .cloned()
            .collect();
        let source = fs::read_to_string(&file)?;
        if !accept(
            &mut edits,
            source::prune_imports(&file, &source, &rewrites, &paths),
        ) {
            eprintln!(
                "adamantium: {}: removing unused imports overlaps another fix; run fix again once it applies",
                file.display()
            );
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
            changed.push(Change {
                file,
                before,
                after,
            });
        }
    }
    if dry_run {
        for change in &changed {
            diff(
                change.file.strip_prefix(&root).unwrap_or(&change.file),
                &change.before,
                &change.after,
            );
        }
    } else {
        // Read and validate every file before writing any file.
        for change in &changed {
            if fs::read_to_string(&change.file)? != change.before {
                bail!("{} changed during fix", change.file.display());
            }
        }
        write(&changed)?;
    }
    let remaining = if dry_run {
        report
    } else {
        // Each rewrite is checked by syntax alone, so only the compiler can
        // tell whether the rewritten project still builds. Fix runs only on a
        // project that built, so any error now is the rewrite's, and a
        // rewrite nothing has verified is undone too.
        match check::check(&root, args, false, strict, None) {
            Ok(report) if report.status != check::Status::Error => report,
            Ok(report) => {
                // The lines are those of the rewritten files.
                for error in &report.errors {
                    match (&error.file, error.line) {
                        (Some(file), Some(line)) => {
                            eprintln!("{file}:{line}: error: {}", error.message);
                        }
                        _ => eprintln!("error: {}", error.message),
                    }
                }
                return restore_all(&changed, "the fixed project no longer builds");
            }
            Err(error) => {
                eprintln!("error: {error:#}");
                return restore_all(&changed, "the fixed project could not be checked");
            }
        }
    };
    for change in &changed {
        eprintln!("adamantium: fixed {}", change.file.display());
    }
    for finding in &remaining.findings {
        eprintln!(
            "{}:{}: {}: {}\n  {}",
            finding.file,
            finding.line.unwrap_or(1),
            finding.rule,
            finding.message,
            Hint::new(&finding.rule, &finding.message)
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

/// One file's text before and after fix rewrites it.
struct Change {
    file: PathBuf,
    before: String,
    after: String,
}

/// Writes every change. If a write fails, the files already written, and the
/// one that failed, get their original text back.
fn write(changes: &[Change]) -> Result<()> {
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = fs::write(&change.file, &change.after) {
            // The failed write may have truncated the file, so it is restored
            // whatever it holds now.
            let mut unrestored = usize::from(!restore(change, true));
            for written in changes.get(..index).unwrap_or_default() {
                unrestored += usize::from(!restore(written, false));
            }
            return Err(error).with_context(|| {
                format!(
                    "writing {}; {}",
                    change.file.display(),
                    restored_note(index + 1, unrestored)
                )
            });
        }
    }
    Ok(())
}

/// Restores every changed file and says why, exiting 2 when all of them have
/// their original text again and failing otherwise.
fn restore_all(changes: &[Change], reason: &str) -> Result<ExitCode> {
    let mut unrestored = 0;
    for change in changes {
        unrestored += usize::from(!restore(change, false));
    }
    let note = restored_note(changes.len(), unrestored);
    if unrestored > 0 {
        bail!("{reason}; {note}");
    }
    eprintln!("adamantium: {reason}, so no fixes were applied; {note}");
    Ok(ExitCode::from(2))
}

/// Writes a file's original text back unless it already has it, naming the
/// file if it can't, and returns whether the file has its original text now.
/// Only a file that still has the text fix wrote is written: one with other
/// text, or one that is gone or can't be read, was changed by something else
/// since, so it is left as it is rather than undoing that change.
/// `interrupted` says fix's own write failed partway, which leaves text that
/// is neither the original nor fix's.
fn restore(change: &Change, interrupted: bool) -> bool {
    let outcome = match fs::read(&change.file) {
        Ok(text) if text == change.before.as_bytes() => return true,
        Ok(text) if interrupted || text == change.after.as_bytes() => {
            fs::write(&change.file, &change.before).map_err(|error| error.to_string())
        }
        Ok(_) => Err("it changed after fix wrote it, so it was left as it is".to_owned()),
        Err(error) => Err(format!(
            "it can't be read ({error}), so it was left as it is"
        )),
    };
    match outcome {
        Ok(()) => {
            eprintln!("adamantium: restored {}", change.file.display());
            true
        }
        Err(reason) => {
            eprintln!(
                "adamantium: could not restore {}: {reason}",
                change.file.display()
            );
            false
        }
    }
}

/// How many of the `changed` files fix wrote now have their original text.
fn restored_note(changed: usize, unrestored: usize) -> String {
    if unrestored == 0 {
        format!("restored the {changed} file(s) it wrote")
    } else {
        format!("could not restore {unrestored} of the {changed} file(s) it wrote")
    }
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

/// The callees in each project file that Clippy resolved to a Tokio channel
/// constructor fix can rewrite, outside macro expansions, by start, with
/// their end and the channel they make.
fn channels(
    diagnostics: &[Value],
    root: &Path,
) -> BTreeMap<PathBuf, BTreeMap<usize, (usize, channels::Kind)>> {
    let mut files: BTreeMap<PathBuf, BTreeMap<usize, (usize, channels::Kind)>> = BTreeMap::new();
    for diagnostic in diagnostics {
        let message = diagnostic["message"].as_str().unwrap_or("");
        let kind = if message.contains("`tokio::sync::oneshot::channel`") {
            channels::Kind::Oneshot
        } else if message.contains("`tokio::sync::watch::channel`") {
            channels::Kind::Watch
        } else if message.contains("`tokio::sync::mpsc::channel`") {
            channels::Kind::Mpsc
        } else {
            continue;
        };
        if diagnostic
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            != Some("clippy::disallowed_methods")
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
                && let Some((start, end)) = range
            {
                files.entry(file).or_default().insert(start, (end, kind));
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
    use crate::lints::{DISALLOWED_MACROS, DISALLOWED_METHODS, DISALLOWED_TYPES};
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
    fn restoring_puts_back_only_text_fix_wrote_and_carries_on_past_failures() {
        let dir = tempfile::tempdir().unwrap();
        let change = |name: &str, current: Option<&str>| {
            let file = dir.path().join(name);
            if let Some(text) = current {
                fs::write(&file, text).unwrap();
            }
            Change {
                file,
                before: "before\n".to_owned(),
                after: "after\n".to_owned(),
            }
        };
        let text = |change: &Change| fs::read_to_string(&change.file).unwrap();

        let rewritten = change("rewritten.rs", Some("after\n"));
        assert!(restore(&rewritten, false));
        assert_eq!(text(&rewritten), "before\n");
        assert!(restore(&rewritten, false));

        // Something else edited the file after fix wrote it.
        let edited = change("edited.rs", Some("edited\n"));
        assert!(!restore(&edited, false));
        assert_eq!(text(&edited), "edited\n");

        // Fix's own write stopped partway.
        let truncated = change("truncated.rs", Some("aft"));
        assert!(restore(&truncated, true));
        assert_eq!(text(&truncated), "before\n");

        // Something else moved the file away after fix wrote it.
        let moved = change("moved.rs", Some("after\n"));
        let elsewhere = dir.path().join("elsewhere.rs");
        fs::rename(&moved.file, &elsewhere).unwrap();
        assert!(!restore(&moved, false));
        assert!(!restore(&moved, true));
        assert!(!moved.file.exists());
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "after\n");

        let unwritable = change("missing/unwritable.rs", None);
        assert!(!restore(&unwritable, false));

        let last = change("last.rs", Some("after\n"));
        let error = restore_all(&[unwritable, last], "the fixed project no longer builds")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the fixed project no longer builds; could not restore 1 of the 2 file(s) it wrote"
        );
        assert_eq!(text(&change("last.rs", None)), "before\n");
    }

    #[test]
    fn a_failed_write_restores_the_files_written_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.rs");
        fs::write(&first, "before\n").unwrap();
        let changes = [
            Change {
                file: first.clone(),
                before: "before\n".to_owned(),
                after: "after\n".to_owned(),
            },
            Change {
                file: dir.path().join("missing/second.rs"),
                before: "before\n".to_owned(),
                after: "after\n".to_owned(),
            },
        ];
        let error = format!("{:#}", write(&changes).unwrap_err());
        assert!(
            error.contains("could not restore 1 of the 2 file(s) it wrote"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&first).unwrap(), "before\n");
    }

    /// The anchors GitHub gives the migration guide's headings: lowercase,
    /// punctuation such as backticks dropped, spaces as hyphens, and a
    /// number after a repeated heading. Lines in code blocks aren't headings.
    fn guide_anchors() -> Result<BTreeSet<String>> {
        let guide =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/migrating.md"))?;
        let mut anchors = BTreeSet::new();
        let mut fence = None;
        for line in guide.lines() {
            let trimmed = line.trim_start();
            let marker = ["```", "~~~"]
                .into_iter()
                .find(|marker| trimmed.starts_with(marker));
            match (fence, marker) {
                (None, Some(marker)) => fence = Some(marker),
                (Some(open), Some(marker)) if open == marker => fence = None,
                _ => {}
            }
            if fence.is_some() || marker.is_some() {
                continue;
            }
            let level = line.chars().take_while(|c| *c == '#').count();
            let Some(text) = line.get(level..).and_then(|text| text.strip_prefix(' ')) else {
                continue;
            };
            if !(1..=6).contains(&level) {
                continue;
            }
            let base: String = text
                .trim()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                .map(|c| if c == ' ' { '-' } else { c })
                .collect();
            let mut anchor = base.clone();
            let mut repeats = 0;
            while !anchors.insert(anchor.clone()) {
                repeats += 1;
                anchor = format!("{base}-{repeats}");
            }
        }
        Ok(anchors)
    }

    /// The paths a list in the bundled default and strict Clippy configurations
    /// disallows.
    fn disallowed_paths(list: &str) -> Result<Vec<String>> {
        let config: toml_edit::DocumentMut = crate::lints::clippy_toml(true)?.parse()?;
        let entries = config
            .get(list)
            .and_then(toml_edit::Item::as_array)
            .with_context(|| format!("clippy.toml has no {list} array"))?;
        entries
            .iter()
            .map(|entry| {
                entry
                    .as_inline_table()
                    .and_then(|entry| entry.get("path"))
                    .and_then(toml_edit::Value::as_str)
                    .map(str::to_owned)
                    .with_context(|| format!("an entry in {list} has no path: {entry}"))
            })
            .collect()
    }

    #[test]
    fn the_guide_anchors_are_its_headings() {
        let anchors = guide_anchors().unwrap();
        for anchor in [
            "shared-state",
            "rwlock",
            "a-lock-held-across-an-await",
            "tasks",
        ] {
            assert!(anchors.contains(anchor), "{anchor} in {anchors:?}");
        }
        // `#[tokio::main]` starts a line in many of its code blocks.
        assert!(
            !anchors.iter().any(|anchor| anchor.contains("tokio")),
            "{anchors:?}"
        );
    }

    #[test]
    fn every_finding_points_at_a_guide_section_or_its_rule_explanation() {
        let strict_config = crate::lints::clippy_toml(true).unwrap();
        let anchors = guide_anchors().unwrap();
        let mut findings: Vec<(String, String)> = crate::lints::selected(true)
            .map(|lint| (lint.qualified(), format!("a finding of {}", lint.name)))
            .chain([(
                crate::lints::OVERFLOW_RULE.to_owned(),
                "overflow checks are off".to_owned(),
            )])
            .chain(
                crate::lints::EXPERIMENTAL_LINTS
                    .iter()
                    .map(|lint| ((*lint).to_owned(), format!("a finding of {lint}"))),
            )
            .collect();
        for (list, rule, kind) in [
            ("disallowed-macros", "clippy::disallowed_macros", "macro"),
            ("disallowed-methods", "clippy::disallowed_methods", "method"),
            ("disallowed-types", "clippy::disallowed_types", "type"),
        ] {
            for path in disallowed_paths(list).unwrap() {
                findings.push((
                    rule.to_owned(),
                    format!("use of a disallowed {kind} `{path}`"),
                ));
            }
        }
        for (rule, message) in &findings {
            let hint = Hint::new(rule, message);
            match hint {
                Hint::Guide(section) => {
                    assert!(anchors.contains(section), "{rule}: {message}: #{section}");
                }
                Hint::Explain(explained) => assert_eq!(explained, rule, "{message}"),
            }
            let remediation = crate::lints::remediation(rule, message, &strict_config)
                .unwrap_or_else(|| panic!("{rule}: {message} has no remediation"));
            assert!(
                remediation.ends_with(&format!(" {hint}.")),
                "{rule}: {message}: {remediation}"
            );
        }
        for reason in [
            state::Reason::AcrossStatements,
            state::Reason::AcrossAwait,
            state::Reason::TwoLocks,
            state::Reason::ControlFlow,
            state::Reason::InsideActor,
            state::Reason::Calls,
            state::Reason::Nested(""),
            state::Reason::Poll,
            state::Reason::Unfollowed(""),
        ] {
            assert!(anchors.contains(reason.section()), "{}", reason.section());
        }
        for kind in [
            channels::Kind::Oneshot,
            channels::Kind::Watch,
            channels::Kind::Mpsc,
        ] {
            assert!(anchors.contains(kind.section()), "{}", kind.section());
        }
    }

    #[test]
    fn every_path_with_a_section_is_disallowed_once() {
        for (list, sections) in [
            ("disallowed-macros", DISALLOWED_MACROS),
            ("disallowed-methods", DISALLOWED_METHODS),
            ("disallowed-types", DISALLOWED_TYPES),
        ] {
            let disallowed = disallowed_paths(list).unwrap();
            let mut seen = BTreeSet::new();
            for path in sections.iter().flat_map(|(_, paths)| paths.iter()) {
                assert!(
                    disallowed.iter().any(|listed| listed == path),
                    "{path} in {list}"
                );
                assert!(seen.insert(path), "{path} twice");
            }
        }
    }

    #[test]
    fn findings_link_by_rule_and_disallowed_path() {
        let method = |path: &str| format!("use of a disallowed method `{path}`");
        let kind = |path: &str| format!("use of a disallowed type `{path}`");
        for (rule, message, hint) in [
            (
                "clippy::disallowed_methods",
                method("std::thread::Builder::spawn"),
                Hint::Guide("blocking-work"),
            ),
            (
                "clippy::disallowed_methods",
                method("tokio::spawn"),
                Hint::Guide("tasks"),
            ),
            (
                "clippy::disallowed_types",
                kind("tokio::sync::Mutex"),
                Hint::Guide("a-lock-held-across-an-await"),
            ),
            (
                "clippy::disallowed_types",
                kind("std::sync::Mutex"),
                Hint::Guide("shared-state"),
            ),
            (
                "clippy::disallowed_types",
                kind("tokio::sync::RwLock"),
                Hint::Guide("rwlock"),
            ),
            (
                "clippy::await_holding_lock",
                "this `MutexGuard` is held across an await point".to_owned(),
                Hint::Guide("a-lock-held-across-an-await"),
            ),
            (
                "clippy::unwrap_used",
                "used `unwrap()` on a `Result` value".to_owned(),
                Hint::Explain("clippy::unwrap_used"),
            ),
            (
                "clippy::disallowed_methods",
                method("std::mem::forget"),
                Hint::Explain("clippy::disallowed_methods"),
            ),
            // A path a project added to its own clippy.toml.
            (
                "clippy::disallowed_methods",
                method("my_crate::spawn"),
                Hint::Explain("clippy::disallowed_methods"),
            ),
            (
                "unsafe_code",
                "usage of an `unsafe` block".to_owned(),
                Hint::Explain("unsafe_code"),
            ),
        ] {
            assert_eq!(Hint::new(rule, &message), hint, "{message}");
        }
        assert_eq!(
            Hint::Explain("clippy::unwrap_used").to_string(),
            "Run cargo adamantium explain clippy::unwrap_used"
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
