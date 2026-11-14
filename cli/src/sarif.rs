//! A check report as a SARIF 2.1.0 log, which GitHub code scanning shows as
//! annotations on pull requests.

use std::{
    env,
    path::{Component, Path, PathBuf, Prefix},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    check::{Finding, Problem, Report, Status, normalize},
    lints::{LINTS, OVERFLOW_RULE},
};

const SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

/// The base that relative artifact URIs resolve against.
const SRCROOT: &str = "%SRCROOT%";

/// Where a report's file names are relative to, and what SARIF URIs are made
/// relative to.
pub struct Roots {
    /// The Cargo workspace root. Clippy names files relative to it, even when
    /// the check ran on a member's directory.
    pub workspace: PathBuf,
    /// The git repository's root when there is one, otherwise the workspace
    /// root. GitHub code scanning resolves relative URIs against the
    /// repository root.
    pub source: PathBuf,
}

impl Roots {
    /// Finds the workspace and repository roots for a project directory.
    pub fn locate(root: &Path) -> Result<Self> {
        let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(root)
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .context("running cargo locate-project")?;
        if !output.status.success() {
            bail!("cargo locate-project could not find the workspace");
        }
        let manifest = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim_end());
        let workspace = manifest
            .parent()
            .context("the workspace manifest has no parent directory")?
            .canonicalize()
            .with_context(|| format!("resolving {}", manifest.display()))?;
        let source = repository(&workspace).unwrap_or_else(|| workspace.clone());
        Ok(Self { workspace, source })
    }
}

/// The root of the git repository holding `dir`, or `None` outside one or
/// without git.
fn repository(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim_end())
        .canonicalize()
        .ok()
}

/// The report as a SARIF log with one run. Each finding is an `error` result
/// for its rule; compiler errors and configuration warnings are tool
/// notifications, so code scanning does not raise alerts for them.
pub fn log(report: &Report, roots: &Roots) -> Value {
    let mut rules: Vec<&str> = Vec::new();
    let results: Vec<Value> = report
        .findings
        .iter()
        .map(|finding| {
            let index = rules
                .iter()
                .position(|rule| *rule == finding.rule)
                .unwrap_or_else(|| {
                    rules.push(&finding.rule);
                    rules.len() - 1
                });
            json!({
                "ruleId": finding.rule,
                "ruleIndex": index,
                "level": "error",
                "message": { "text": finding.message },
                "locations": [location(&finding_path(finding, report, roots), finding.line, finding.column, &roots.source)],
            })
        })
        .collect();
    let notifications: Vec<Value> = report
        .errors
        .iter()
        .map(|problem| notification(problem, "error", roots))
        .chain(
            report
                .warnings
                .iter()
                .map(|problem| notification(problem, "warning", roots)),
        )
        .collect();
    json!({
        "$schema": SCHEMA,
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "cargo-adamantium",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": env!("CARGO_PKG_REPOSITORY"),
                    "rules": rules.iter().map(|rule| descriptor(rule)).collect::<Vec<_>>(),
                },
            },
            "originalUriBaseIds": {
                SRCROOT: { "uri": directory_uri(&roots.source) },
            },
            // rustc counts columns in characters.
            "columnKind": "unicodeCodePoints",
            "invocations": [{
                "executionSuccessful": report.status != Status::Error,
                "toolExecutionNotifications": notifications,
            }],
            "results": results,
            "properties": { "skipped": report.skipped },
        }],
    })
}

/// The finding's file as an absolute path. The overflow check reads the
/// project's own `Cargo.toml`; every other file name comes from Clippy.
fn finding_path(finding: &Finding, report: &Report, roots: &Roots) -> PathBuf {
    let base = if finding.rule == OVERFLOW_RULE {
        &report.root
    } else {
        &roots.workspace
    };
    PathBuf::from(normalize(&base.join(&finding.file).to_string_lossy()))
}

/// What GitHub shows for a rule: its reason, and Clippy's documentation for
/// Clippy lints.
fn descriptor(rule: &str) -> Value {
    let why = if rule == OVERFLOW_RULE {
        "the release profile must set overflow-checks = true, so integer overflow panics instead of wrapping"
    } else {
        LINTS
            .iter()
            .find(|lint| lint.qualified() == rule)
            .map_or("a rule of the adamantium fragment", |lint| lint.why)
    };
    let mut descriptor = json!({
        "id": rule,
        "shortDescription": { "text": why },
        "fullDescription": { "text": why },
        "help": { "text": why },
        "defaultConfiguration": { "level": "error" },
    });
    if let (Some(name), Some(fields)) = (rule.strip_prefix("clippy::"), descriptor.as_object_mut())
    {
        fields.insert(
            "helpUri".to_owned(),
            json!(format!(
                "https://rust-lang.github.io/rust-clippy/master/index.html#{name}"
            )),
        );
    }
    descriptor
}

fn notification(problem: &Problem, level: &str, roots: &Roots) -> Value {
    let mut notification = json!({
        "level": level,
        "message": { "text": problem.message },
    });
    // A file named `clippy.toml` is Adamantium's bundled configuration, which
    // is not in the repository.
    let file = problem
        .file
        .as_deref()
        .filter(|file| *file != "clippy.toml");
    if let (Some(file), Some(fields)) = (file, notification.as_object_mut()) {
        let path = PathBuf::from(normalize(&roots.workspace.join(file).to_string_lossy()));
        fields.insert(
            "locations".to_owned(),
            json!([location(&path, problem.line, None, &roots.source)]),
        );
    }
    notification
}

/// A physical location. GitHub requires a region, so a finding about a whole
/// file points at its first line.
fn location(path: &Path, line: Option<u64>, column: Option<u64>, source: &Path) -> Value {
    let artifact = match path.strip_prefix(source) {
        Ok(relative) => json!({ "uri": relative_uri(relative), "uriBaseId": SRCROOT }),
        Err(_) => json!({ "uri": file_uri(path) }),
    };
    let line = line.unwrap_or(1);
    let region = match column {
        Some(column) => json!({ "startLine": line, "startColumn": column }),
        None => json!({ "startLine": line }),
    };
    json!({ "physicalLocation": { "artifactLocation": artifact, "region": region } })
}

/// A relative path as a URI reference with `/` separators.
fn relative_uri(path: &Path) -> String {
    let mut uri = String::new();
    for component in path.components() {
        if !uri.is_empty() {
            uri.push('/');
        }
        encode(&component.as_os_str().to_string_lossy(), &mut uri);
    }
    uri
}

/// An absolute path as a `file` URI.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    uri.push('/');
                    uri.push(char::from(letter));
                    uri.push(':');
                }
                _ => {
                    uri.push('/');
                    encode(&prefix.as_os_str().to_string_lossy(), &mut uri);
                }
            },
            other => {
                uri.push('/');
                encode(&other.as_os_str().to_string_lossy(), &mut uri);
            }
        }
    }
    uri
}

/// A directory as a `file` URI ending in `/`, as SARIF requires of a base.
fn directory_uri(path: &Path) -> String {
    let mut uri = file_uri(path);
    if !uri.ends_with('/') {
        uri.push('/');
    }
    uri
}

/// Percent-encodes one path segment, keeping only unreserved characters.
fn encode(segment: &str, uri: &mut String) {
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn finding(rule: &str, file: &str, line: Option<u64>) -> Finding {
        Finding {
            rule: rule.to_owned(),
            message: format!("{rule} broken"),
            file: file.to_owned(),
            line,
            column: line.map(|_| 5),
        }
    }

    fn report(root: &str, findings: Vec<Finding>) -> Report {
        Report {
            schema_version: 2,
            root: PathBuf::from(root),
            status: Status::Findings,
            findings,
            skipped: BTreeMap::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn roots(workspace: &str, source: &str) -> Roots {
        Roots {
            workspace: PathBuf::from(workspace),
            source: PathBuf::from(source),
        }
    }

    fn artifact(result: &Value) -> &Value {
        &result["locations"][0]["physicalLocation"]["artifactLocation"]
    }

    #[test]
    fn uris_are_relative_to_the_repository_root() {
        let report = report(
            "/repo/ws/member",
            vec![
                finding("clippy::unwrap_used", "member/src/lib.rs", Some(3)),
                finding(OVERFLOW_RULE, "Cargo.toml", None),
            ],
        );
        let log = log(&report, &roots("/repo/ws", "/repo"));
        let run = &log["runs"][0];
        assert_eq!(
            run["originalUriBaseIds"][SRCROOT]["uri"],
            json!("file:///repo/")
        );
        let results = run["results"].as_array().unwrap();
        assert_eq!(
            artifact(&results[0]),
            &json!({ "uri": "ws/member/src/lib.rs", "uriBaseId": SRCROOT })
        );
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["region"],
            json!({ "startLine": 3, "startColumn": 5 })
        );
        // The overflow check reads the checked directory's manifest.
        assert_eq!(
            artifact(&results[1]),
            &json!({ "uri": "ws/member/Cargo.toml", "uriBaseId": SRCROOT })
        );
        assert_eq!(
            results[1]["locations"][0]["physicalLocation"]["region"],
            json!({ "startLine": 1 })
        );
    }

    #[test]
    fn files_outside_the_repository_get_absolute_uris() {
        let report = report(
            "/repo",
            vec![
                finding("clippy::panic", "../shared dir/a#b.rs", Some(1)),
                finding("clippy::panic", "src/naïve 100%.rs", Some(2)),
            ],
        );
        let log = log(&report, &roots("/repo", "/repo"));
        let results = log["runs"][0]["results"].as_array().unwrap();
        assert_eq!(
            artifact(&results[0]),
            &json!({ "uri": "file:///shared%20dir/a%23b.rs" })
        );
        assert_eq!(
            artifact(&results[1]),
            &json!({ "uri": "src/na%C3%AFve%20100%25.rs", "uriBaseId": SRCROOT })
        );
    }

    #[test]
    fn each_rule_is_described_once_and_results_point_at_it() {
        let report = report(
            "/repo",
            vec![
                finding("clippy::unwrap_used", "src/a.rs", Some(1)),
                finding("unsafe_code", "src/a.rs", Some(2)),
                finding("clippy::unwrap_used", "src/b.rs", Some(3)),
            ],
        );
        let log = log(&report, &roots("/repo", "/repo"));
        let run = &log["runs"][0];
        let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
        let ids: Vec<&str> = rules
            .iter()
            .map(|rule| rule["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["clippy::unwrap_used", "unsafe_code"]);
        assert_eq!(
            rules[0]["helpUri"],
            "https://rust-lang.github.io/rust-clippy/master/index.html#unwrap_used"
        );
        assert_eq!(rules[1].get("helpUri"), None);
        assert_eq!(
            rules[1]["shortDescription"]["text"],
            "every guarantee below rests on safe Rust"
        );
        let indexes: Vec<u64> = run["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["ruleIndex"].as_u64().unwrap())
            .collect();
        assert_eq!(indexes, [0, 1, 0]);
    }

    #[test]
    fn errors_and_warnings_are_notifications_not_results() {
        let mut report = report("/repo", Vec::new());
        report.status = Status::Error;
        report.errors.push(Problem {
            message: "mismatched types".to_owned(),
            file: Some("src/lib.rs".to_owned()),
            line: Some(4),
        });
        report.warnings.push(Problem {
            message: "unknown key".to_owned(),
            file: Some("clippy.toml".to_owned()),
            line: Some(2),
        });
        report.warnings.push(Problem {
            message: "unknown lint".to_owned(),
            file: None,
            line: None,
        });
        let log = log(&report, &roots("/repo", "/repo"));
        let run = &log["runs"][0];
        assert_eq!(run["results"], json!([]));
        assert_eq!(
            run["invocations"],
            json!([{
                "executionSuccessful": false,
                "toolExecutionNotifications": [
                    {
                        "level": "error",
                        "message": { "text": "mismatched types" },
                        "locations": [{ "physicalLocation": {
                            "artifactLocation": { "uri": "src/lib.rs", "uriBaseId": SRCROOT },
                            "region": { "startLine": 4 },
                        } }],
                    },
                    { "level": "warning", "message": { "text": "unknown key" } },
                    { "level": "warning", "message": { "text": "unknown lint" } },
                ],
            }])
        );
    }
}
