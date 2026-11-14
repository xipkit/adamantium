use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn adamantium() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn check_only_reports_arithmetic_in_the_strict_tier() {
    let marked: Vec<u64> = include_str!("fixtures/strict/src/lib.rs")
        .lines()
        .enumerate()
        .filter(|(_, line)| line.ends_with("// strict"))
        .map(|(index, _)| u64::try_from(index + 1).unwrap())
        .collect();
    assert_eq!(marked.len(), 3);
    for strict in [false, true] {
        let mut command = adamantium();
        command
            .args(["check", "--format", "json"])
            .arg(fixture("strict"));
        if strict {
            command.arg("--strict");
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(i32::from(strict)), "{stderr}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["errors"], serde_json::json!([]), "{report}");
        assert_eq!(report["warnings"], serde_json::json!([]), "{report}");
        let findings = report["findings"].as_array().unwrap();
        if strict {
            let mut lines: Vec<u64> = findings
                .iter()
                .map(|finding| {
                    assert_eq!(finding["rule"], "clippy::arithmetic_side_effects");
                    assert_eq!(finding["file"], "src/lib.rs");
                    finding["line"].as_u64().unwrap()
                })
                .collect();
            lines.sort_unstable();
            assert_eq!(lines, marked, "{report}");
        } else {
            assert!(findings.is_empty(), "{report}");
        }
    }
}

#[test]
fn strict_findings_have_text_and_sarif_reports() {
    let output = adamantium()
        .args(["check", "--strict"])
        .arg(fixture("strict"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("#arithmetic_side_effects"), "{stderr}");
    assert!(stderr.contains("3 findings in src/lib.rs"), "{stderr}");

    let output = adamantium()
        .args(["check", "--strict", "--format", "sarif"])
        .arg(fixture("strict"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let log: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(sarif_schema_errors(&log).unwrap().is_empty(), "{log}");
    let run = &log["runs"][0];
    assert_eq!(run["results"].as_array().unwrap().len(), 3, "{log}");
    let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{log}");
    assert_eq!(rules[0]["id"], "clippy::arithmetic_side_effects");
    assert!(
        rules[0]["shortDescription"]["text"]
            .as_str()
            .unwrap()
            .contains("checked, saturating or wrapping"),
        "{log}"
    );
    assert!(
        rules[0]["helpUri"]
            .as_str()
            .unwrap()
            .ends_with("#arithmetic_side_effects")
    );
}

/// A standalone crate with unchecked arithmetic and its checked alternatives.
fn strict_project(workspace: bool) -> io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join("src"))?;
    fs::write(
        dir.path().join("src/lib.rs"),
        include_str!("fixtures/strict/src/lib.rs"),
    )?;
    let mut manifest = fs::read_to_string(fixture("strict").join("Cargo.toml"))?;
    if !workspace {
        manifest = manifest.replace("[workspace]\n", "");
    }
    fs::write(dir.path().join("Cargo.toml"), manifest)?;
    Ok(dir)
}

#[test]
fn init_strict_upgrades_packages_and_workspaces_and_is_idempotent() {
    for workspace in [false, true] {
        let dir = strict_project(workspace).unwrap();
        let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = dir.path().join("Cargo.toml");
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("arithmetic_side_effects")
        );

        let output = adamantium()
            .args(["init", "--strict"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest = fs::read_to_string(&path).unwrap();
        let doc: toml_edit::DocumentMut = manifest.parse().unwrap();
        let parent = if workspace {
            &doc["workspace"]
        } else {
            doc.as_item()
        };
        assert_eq!(
            parent["lints"]["clippy"]["arithmetic_side_effects"].as_str(),
            Some("deny")
        );
        assert_eq!(
            parent["lints"]["clippy"]["unwrap_used"].as_str(),
            Some("deny")
        );
        assert_eq!(
            parent["lints"]["rust"]["unsafe_code"].as_str(),
            Some("forbid")
        );

        for strict in [true, false] {
            let mut command = adamantium();
            command.arg("init").arg(dir.path());
            if strict {
                command.arg("--strict");
            }
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("already has the rules"));
            assert_eq!(fs::read_to_string(&path).unwrap(), manifest);
        }

        let output = Command::new("cargo")
            .current_dir(dir.path())
            .args(["clippy", "--message-format=json"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        let findings: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["message"]["code"]["code"] == "clippy::arithmetic_side_effects")
            .collect();
        assert_eq!(findings.len(), 3, "{stderr}");
        assert!(
            findings
                .iter()
                .all(|finding| finding["message"]["level"] == "error")
        );
    }
}

#[test]
fn strict_rules_respect_allow_exceptions_and_skipped_paths() {
    for skip_list in [
        "allow = [\"arithmetic_side_effects\"]",
        "allow = [\"clippy::arithmetic_side_effects\"]",
        "exceptions = { arithmetic_side_effects = [\"src/\"] }",
        "skip = [\"src/\"]",
    ] {
        let dir = strict_project(false).unwrap();
        let path = dir.path().join("Cargo.toml");
        let manifest = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            format!("{manifest}\n[package.metadata.adamantium]\n{skip_list}\n"),
        )
        .unwrap();
        let output = adamantium()
            .args(["init", "--strict"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["lints"]["clippy"]
                .get("arithmetic_side_effects")
                .is_some(),
            skip_list.starts_with("skip"),
            "{doc}"
        );
        let output = adamantium()
            .args(["check", "--strict", "--format", "json"])
            .arg(dir.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["findings"], serde_json::json!([]), "{report}");
        assert_eq!(
            report["skipped"],
            serde_json::json!({"clippy::arithmetic_side_effects": 3}),
            "{report}"
        );
    }
}

#[test]
fn check_reports_every_rule_the_violations_fixture_breaks() {
    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(fixture("violations"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 3, "{stderr}");
    assert_eq!(report["warnings"], serde_json::json!([]), "{stderr}");
    assert_eq!(report["status"], "findings", "{stderr}");
    assert_eq!(report["errors"], serde_json::json!([]), "{stderr}");
    let rules: HashSet<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["rule"].as_str().unwrap())
        .collect();
    for expected in [
        "unsafe_code",
        "unused_must_use",
        "clippy::let_underscore_future",
        "clippy::let_underscore_must_use",
        "clippy::unwrap_used",
        "clippy::expect_used",
        "clippy::indexing_slicing",
        "clippy::string_slice",
        "clippy::panic",
        "clippy::cast_possible_truncation",
        "clippy::mem_forget",
        "clippy::disallowed_methods",
        "clippy::disallowed_types",
        "clippy::dbg_macro",
        "clippy::exit",
        "clippy::await_holding_lock",
        "clippy::future_not_send",
        "adamantium::overflow_checks",
    ] {
        assert!(
            rules.contains(expected),
            "no finding for {expected} in {rules:?}"
        );
    }
    for (rule, path) in [
        ("clippy::disallowed_methods", "std::process::abort"),
        ("clippy::disallowed_types", "std::mem::ManuallyDrop"),
    ] {
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| {
                    finding["rule"] == rule && finding["message"].as_str().unwrap().contains(path)
                }),
            "no {rule} finding for {path}: {report}"
        );
    }
    let first = &report["findings"][0];
    assert!(first["file"].as_str().unwrap().ends_with(".rs"));
    assert!(first["line"].as_u64().is_some());
    // Each finding carries the trimmed text of the line it starts on, so a
    // baseline can match it after the line moves.
    for finding in report["findings"].as_array().unwrap() {
        let Some(line) = finding["line"].as_u64() else {
            assert_eq!(finding["source"], serde_json::Value::Null, "{finding}");
            continue;
        };
        let text =
            fs::read_to_string(fixture("violations").join(finding["file"].as_str().unwrap()))
                .unwrap();
        let expected = text
            .lines()
            .nth(usize::try_from(line - 1).unwrap())
            .unwrap()
            .trim();
        assert_eq!(finding["source"], expected, "{finding}");
    }
    // The binary depends on the library. Its finding is only reported when
    // the library's findings do not fail the build and skip what depends on it.
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["file"].as_str().unwrap().ends_with("src/main.rs")),
        "the binary target was not checked: {rules:?}"
    );
}

/// Each fixture must produce a disallowed method finding on exactly the lines
/// of its `src` files that end in `// disallowed`, and on no others. `runtime`
/// covers Tokio's runtime entry points; `unstable` covers `task::Builder`,
/// which needs `--cfg tokio_unstable`; `indexing` covers panicking index methods
/// beside their checked alternatives; `channels` covers unbounded constructors
/// beside their bounded alternatives; `time` covers clock entry points beside
/// Adamantium's wrappers and wall-clock measurements.
/// `spawns` covers async-std and smol's spawn entry points beside runtime entry.
#[test]
fn check_reports_disallowed_methods() {
    for name in [
        "runtime", "unstable", "indexing", "channels", "time", "spawns",
    ] {
        let root = fixture(name);
        let mut marked = Vec::new();
        let mut directories = vec![root.join("src")];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                    continue;
                }
                let file = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                for (index, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
                    if line.ends_with("// disallowed") {
                        marked.push((file.clone(), u64::try_from(index + 1).unwrap()));
                    }
                }
            }
        }
        marked.sort();
        assert!(!marked.is_empty(), "{name} marks no lines");

        let output = adamantium()
            .args(["adamantium", "check", "--format", "json"])
            .arg(&root)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["warnings"], serde_json::json!([]), "{stderr}");
        assert_eq!(report["errors"], serde_json::json!([]), "{stderr}");
        let mut reported: Vec<(String, u64)> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["rule"] == "clippy::disallowed_methods")
            .map(|finding| {
                (
                    finding["file"].as_str().unwrap().to_owned(),
                    finding["line"].as_u64().unwrap(),
                )
            })
            .collect();
        reported.sort();
        assert_eq!(reported, marked, "{name}: {stderr}");
    }
}

/// Checks the fixture `name` and asserts that it produces a finding on exactly
/// the lines of `src/lib.rs` that end in `// disallowed_types` or
/// `// disallowed_methods`, for that rule, and no other finding. Returns the
/// findings.
fn check_marked_rules(name: &str) -> TestResult<Vec<serde_json::Value>> {
    let root = fixture(name);
    let mut marked = Vec::new();
    for (index, line) in fs::read_to_string(root.join("src/lib.rs"))?
        .lines()
        .enumerate()
    {
        for rule in ["disallowed_types", "disallowed_methods"] {
            if line.ends_with(&format!("// {rule}")) {
                marked.push((format!("clippy::{rule}"), u64::try_from(index + 1)?));
            }
        }
    }
    marked.sort();
    assert!(!marked.is_empty(), "{name} marks no lines");

    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(&root)
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let none = serde_json::json!([]);
    assert_eq!(report.get("warnings"), Some(&none), "{stderr}");
    assert_eq!(report.get("errors"), Some(&none), "{stderr}");
    let findings = report
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .ok_or("findings is not an array")?
        .clone();
    let mut reported = Vec::new();
    for finding in &findings {
        let field = |key| finding.get(key).ok_or(format!("{finding} has no {key}"));
        assert_eq!(field("file")?, "src/lib.rs", "{finding}");
        let rule = field("rule")?.as_str().ok_or("a rule that is not text")?;
        let line = field("line")?
            .as_u64()
            .ok_or("a line that is not a number")?;
        reported.push((rule.to_owned(), line));
    }
    reported.sort();
    reported.dedup();
    assert_eq!(reported, marked, "{name}: {stderr}");
    Ok(findings)
}

/// Tokio's broadcast channel is reported; a `Semaphore` permit held across an
/// await or forgotten, `watch`, and one `Channel` per receiver are not.
#[test]
fn check_reports_broadcast_and_allows_semaphore() {
    for finding in check_marked_rules("sync").unwrap() {
        assert!(
            finding["message"]
                .as_str()
                .unwrap()
                .contains("tokio::sync::broadcast::"),
            "{finding}"
        );
    }
}

/// Direct calls and imported aliases are reported, while structured deadlines
/// and reasoned local exceptions remain allowed.
#[test]
fn check_reports_timeouts_and_allows_structured_deadlines() {
    for finding in check_marked_rules("cancellation").unwrap() {
        assert_eq!(
            finding["message"], "use of a disallowed method `tokio::time::timeout`",
            "{finding}"
        );
    }
}

/// Every access method of a `thread_local!` value is reported, called on the
/// static or through a `&LocalKey`, unless a local allow exempts it. Atomics,
/// `OnceLock` and `LazyLock` statics, and a static `Actor` are not.
#[test]
fn check_reports_thread_locals_and_allows_atomics_and_statics() {
    let mut methods: Vec<String> = check_marked_rules("state")
        .unwrap()
        .iter()
        .filter(|finding| finding["rule"] == "clippy::disallowed_methods")
        .map(|finding| finding["message"].as_str().unwrap().to_owned())
        .collect();
    methods.sort();
    methods.dedup();
    let mut expected: Vec<String> = [
        "get",
        "replace",
        "set",
        "take",
        "try_with",
        "with",
        "with_borrow",
        "with_borrow_mut",
    ]
    .iter()
    .map(|method| format!("use of a disallowed method `std::thread::LocalKey::{method}`"))
    .collect();
    expected.sort();
    assert_eq!(methods, expected);
}

#[test]
fn check_covers_every_workspace_member() {
    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(fixture("workspace"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["warnings"], serde_json::json!([]), "{stderr}");
    let files: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["file"].as_str().unwrap())
        .collect();
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("src/lib.rs") && !file.contains("member")),
        "the root package was not checked: {files:?}"
    );
    assert!(
        files.iter().any(|file| file.ends_with("member/src/lib.rs")),
        "the member package was not checked: {files:?}"
    );
    // The root includes shared.rs through `#[path = "../shared.rs"]`.
    assert!(
        files.contains(&"shared.rs"),
        "the included file is not reported under its plain name: {files:?}"
    );
    assert!(
        files.iter().all(|file| !file.contains("..")),
        "a path still has a parent segment: {files:?}"
    );
}

#[test]
fn check_applies_the_skip_list() {
    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(fixture("skips"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["warnings"], serde_json::json!([]), "{stderr}");
    let mut found: Vec<(String, String)> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| {
            (
                finding["rule"].as_str().unwrap().to_owned(),
                finding["file"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            ("clippy::string_slice".to_owned(), "src/lib.rs".to_owned()),
            ("clippy::unwrap_used".to_owned(), "src/lib.rs".to_owned()),
        ],
        "{stderr}"
    );
    assert_eq!(
        report["skipped"],
        serde_json::json!({
            "clippy::cast_possible_truncation": 1,
            "clippy::indexing_slicing": 1,
            "clippy::string_slice": 1,
        }),
        "{stderr}"
    );

    let output = adamantium()
        .args(["adamantium", "check"])
        .arg(fixture("skips"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("2 findings in src/lib.rs (3 skipped by the skip list in Cargo.toml)"),
        "{stderr}"
    );
}

/// Runs `check` on `dir` with `args`, returning its exit code, stdout and
/// stderr.
fn check_with(dir: &Path, args: &[&str]) -> io::Result<(Option<i32>, Vec<u8>, String)> {
    let output = adamantium()
        .args(["adamantium", "check"])
        .args(args)
        .arg(dir)
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status.code(), output.stdout, stderr))
}

#[test]
fn check_reports_recursive_macros_at_each_caller() {
    let fixture = fixture("macros");
    let (code, stdout, stderr) = check_with(&fixture, &["--format", "json"]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["errors"], serde_json::json!([]), "{report}");
    assert_eq!(report["warnings"], serde_json::json!([]), "{report}");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 3, "{report}");
    let mut macro_lines = Vec::new();
    for finding in findings {
        assert_eq!(finding["file"], "src/lib.rs", "{report}");
        assert_eq!(finding["column"], 5, "{report}");
        if finding["rule"] == "clippy::disallowed_methods" {
            assert_eq!(finding["source"], "recursive_macros::sleep!();", "{report}");
            macro_lines.push(finding["line"].as_u64().unwrap());
        } else {
            assert_eq!(finding["rule"], "clippy::unwrap_used", "{report}");
            assert_eq!(finding["line"], 8, "{report}");
            assert_eq!(finding["source"], "value.unwrap()", "{report}");
        }
    }
    macro_lines.sort_unstable();
    assert_eq!(macro_lines, [2, 5], "{report}");
    assert_eq!(
        report["skipped"],
        serde_json::json!({ "clippy::disallowed_methods": 1 }),
        "{report}"
    );

    let (code, _, stderr) = check_with(&fixture, &[]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    for line in macro_lines {
        assert!(
            stderr.contains(&format!("--> src/lib.rs:{line}:5")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("--> src/skipped.rs:"), "{stderr}");
    assert!(
        stderr.contains("adamantium: 3 findings in src/lib.rs"),
        "{stderr}"
    );

    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.json");
    fs::write(&baseline, stdout).unwrap();
    let (code, stdout, stderr) = check_with(
        &fixture,
        &["--format", "json", "--baseline", baseline.to_str().unwrap()],
    )
    .unwrap();
    assert_eq!(code, Some(0), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["status"], "clean", "{report}");
    assert_eq!(report["findings"], serde_json::json!([]), "{report}");
    assert_eq!(
        report["skipped"],
        serde_json::json!({ "clippy::disallowed_methods": 1 }),
        "{report}"
    );
    assert_eq!(
        report["baselined"],
        serde_json::json!({ "clippy::disallowed_methods": 2, "clippy::unwrap_used": 1 }),
        "{report}"
    );
    assert_eq!(report["baseline_unmatched"], 0, "{report}");
}

#[test]
fn check_with_a_baseline_fails_only_on_new_findings() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    // No overflow-checks, so the baseline also covers a finding without a line.
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let lib = dir.path().join("src/lib.rs");
    fs::write(
        &lib,
        "pub fn first(v: Option<i32>) -> i32 {\n    v.unwrap()\n}\n",
    )
    .unwrap();
    let (code, stdout, stderr) = check_with(dir.path(), &["--format", "json"]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    let baseline = dir.path().join("baseline.json");
    fs::write(&baseline, stdout).unwrap();
    let baseline = baseline.to_str().unwrap();

    // Code added above the old finding moves and reindents it, and adds one.
    fs::write(
        &lib,
        "pub fn second(v: Option<i32>) -> i32 {\n    v.expect(\"set\")\n}\n\npub mod inner {\n    pub fn first(v: Option<i32>) -> i32 {\n        v.unwrap()\n    }\n}\n",
    )
    .unwrap();
    let (code, stdout, stderr) =
        check_with(dir.path(), &["--format", "json", "--baseline", baseline]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["status"], "findings", "{report}");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["rule"], "clippy::expect_used", "{report}");
    assert_eq!(findings[0]["line"], 2, "{report}");
    assert_eq!(
        report["baselined"],
        serde_json::json!({
            "adamantium::overflow_checks": 1,
            "clippy::unwrap_used": 1,
        }),
        "{report}"
    );
    assert_eq!(report["baseline_unmatched"], 0, "{report}");

    // Text output shows only the new finding's diagnostic.
    let (code, _, stderr) = check_with(dir.path(), &["--baseline", baseline]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("expect_used"), "{stderr}");
    assert!(!stderr.contains("unwrap_used"), "{stderr}");
    assert!(!stderr.contains("overflow-checks"), "{stderr}");
    assert!(
        stderr.contains("adamantium: 1 finding in src/lib.rs (2 in the baseline)"),
        "{stderr}"
    );

    // Fixing both leaves the check clean and says the baseline is stale.
    fs::write(
        &lib,
        "pub fn first(v: Option<i32>) -> i32 {\n    v.unwrap_or(0)\n}\n",
    )
    .unwrap();
    let (code, stdout, stderr) =
        check_with(dir.path(), &["--format", "json", "--baseline", baseline]).unwrap();
    assert_eq!(code, Some(0), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["status"], "clean", "{report}");
    assert_eq!(report["baseline_unmatched"], 1, "{report}");
    let (code, _, stderr) = check_with(dir.path(), &["--baseline", baseline]).unwrap();
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("adamantium: clean (1 in the baseline)\nadamantium: 1 finding in the baseline no longer occurs"),
        "{stderr}"
    );

    // Without a baseline, every finding counts and none is matched.
    let (code, stdout, stderr) = check_with(dir.path(), &["--format", "json"]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["baselined"], serde_json::json!({}), "{report}");
    assert_eq!(
        report["baseline_unmatched"],
        serde_json::Value::Null,
        "{report}"
    );
}

#[test]
fn check_with_a_baseline_keeps_the_skip_list() {
    let (code, stdout, stderr) = check_with(&fixture("skips"), &["--format", "json"]).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.json");
    fs::write(&baseline, stdout).unwrap();
    let (code, stdout, stderr) = check_with(
        &fixture("skips"),
        &["--format", "json", "--baseline", baseline.to_str().unwrap()],
    )
    .unwrap();
    assert_eq!(code, Some(0), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(report["findings"], serde_json::json!([]), "{report}");
    assert_eq!(
        report["skipped"],
        serde_json::json!({
            "clippy::cast_possible_truncation": 1,
            "clippy::indexing_slicing": 1,
            "clippy::string_slice": 1,
        }),
        "{report}"
    );
    assert_eq!(
        report["baselined"],
        serde_json::json!({
            "clippy::string_slice": 1,
            "clippy::unwrap_used": 1,
        }),
        "{report}"
    );
}

#[test]
fn check_refuses_an_unusable_baseline_before_running_clippy() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("old.json");
    fs::write(
        &baseline,
        r#"{"schema_version": 2, "root": "/p", "status": "findings", "findings": []}"#,
    )
    .unwrap();
    let missing = dir.path().join("missing.json");
    for (path, expected) in [
        (&baseline, "it has schema version 2"),
        (&missing, "reading"),
    ] {
        let (code, stdout, stderr) = check_with(
            &fixture("clean"),
            &["--format", "json", "--baseline", path.to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stdout.is_empty());
        assert!(stderr.starts_with("error: "), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!stderr.contains("Checking"), "{stderr}");
    }
}

#[test]
fn check_rejects_an_unknown_rule_in_the_skip_list() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "").unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[package.metadata.adamantium]\nallow = [\"no_such_rule\"]\n",
    )
    .unwrap();
    let output = adamantium().arg("check").arg(dir.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("unknown rule `no_such_rule`"), "{stderr}");
}

#[test]
fn init_leaves_out_rules_the_skip_list_allows() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[package.metadata.adamantium]\nskip = [\"tests/\"]\nallow = [\"indexing_slicing\", \"overflow_checks\"]\n\n[package.metadata.adamantium.exceptions]\nstring_slice = [\"src/text/\"]\n",
    )
    .unwrap();
    let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let manifest = fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("unwrap_used = \"deny\""), "{manifest}");
    assert!(manifest.contains("unsafe_code = \"forbid\""), "{manifest}");
    assert!(
        !manifest.contains("indexing_slicing = \"deny\""),
        "{manifest}"
    );
    assert!(!manifest.contains("string_slice = \"deny\""), "{manifest}");
    assert!(!manifest.contains("overflow-checks"), "{manifest}");
    assert!(
        stderr.contains("only apply to `cargo adamantium check`"),
        "{stderr}"
    );
}

#[test]
fn check_passes_the_clean_fixture_and_prints_a_summary() {
    let output = adamantium()
        .args(["check"])
        .arg(fixture("clean"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("adamantium: clean"), "{stderr}");
    assert!(output.stdout.is_empty());

    let output = adamantium()
        .args(["check", "--format", "json"])
        .arg(fixture("clean"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["status"], "clean");
    assert_eq!(report["findings"], serde_json::json!([]));
    assert_eq!(report["warnings"], serde_json::json!([]));
    assert_eq!(report["errors"], serde_json::json!([]));
}

#[test]
fn check_reports_spanless_lint_warnings_once_in_json_and_text() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "").unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.release]\noverflow-checks = true\n",
    )
    .unwrap();
    let messages = [
        "unknown lint: `clippy::adamantium_missing_lint`",
        "unknown lint: `clippy::adamantium_other_missing_lint`",
        "lint `unaligned_references` has been removed",
    ];
    for format in ["json", "text"] {
        let output = adamantium()
            .args(["check", "--format", format])
            .arg(dir.path())
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                "-Wclippy::adamantium_missing_lint\x1f-Wclippy::adamantium_other_missing_lint\x1f-Wunaligned_references",
            )
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{format}: {stderr}");
        if format == "json" {
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["schema_version"], 3, "{report}");
            assert_eq!(report["status"], "clean", "{report}");
            assert_eq!(report["findings"], serde_json::json!([]), "{report}");
            assert_eq!(report["errors"], serde_json::json!([]), "{report}");
            let warnings = report["warnings"].as_array().unwrap();
            assert_eq!(warnings.len(), messages.len(), "{report}");
            for message in messages {
                assert!(
                    warnings
                        .iter()
                        .any(|warning| warning["message"].as_str().unwrap().starts_with(message)),
                    "{report}"
                );
            }
            for warning in warnings {
                assert_eq!(warning["file"], serde_json::Value::Null, "{warning}");
                assert_eq!(warning["line"], serde_json::Value::Null, "{warning}");
            }
        } else {
            assert!(output.stdout.is_empty());
            assert!(stderr.contains("adamantium: clean"), "{stderr}");
            for message in messages {
                assert_eq!(stderr.matches(message).count(), 1, "{stderr}");
            }
        }
    }
}

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Runs `check --format sarif` on `dir`, passing `cargo_args` on to Clippy,
/// and returns its exit code and log, or an error if the log does not match
/// the SARIF 2.1.0 schema.
fn check_sarif(
    dir: &Path,
    env: &[(&str, &Path)],
    cargo_args: &[&str],
) -> TestResult<(Option<i32>, serde_json::Value)> {
    let mut command = adamantium();
    command
        .args(["adamantium", "check", "--format", "sarif"])
        .arg(dir)
        .arg("--")
        .args(cargo_args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let log: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|error| format!("{error}: {stderr}"))?;
    let errors = sarif_schema_errors(&log)?;
    if !errors.is_empty() {
        return Err(format!("{errors:#?}\n{log:#}").into());
    }
    Ok((output.status.code(), log))
}

/// Where `log` breaks the SARIF 2.1.0 schema. The schema is SchemaStore's
/// copy of the OASIS schema, which declares draft 7, so that URI formats are
/// checked.
fn sarif_schema_errors(log: &serde_json::Value) -> TestResult<Vec<String>> {
    let schema: serde_json::Value = serde_json::from_str(include_str!("sarif-schema-2.1.0.json"))?;
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)?;
    Ok(validator
        .iter_errors(log)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect())
}

/// The file a result's location names, resolved against the run's base URI.
fn sarif_path(run: &serde_json::Value, result: &serde_json::Value) -> Option<PathBuf> {
    let artifact = result.pointer("/locations/0/physicalLocation/artifactLocation")?;
    let uri = artifact.get("uri")?.as_str()?;
    let uri = match artifact.get("uriBaseId") {
        Some(base) => {
            let base = run
                .get("originalUriBaseIds")?
                .get(base.as_str()?)?
                .get("uri")?
                .as_str()?;
            // A base URI must end in `/` to resolve relative references.
            base.ends_with('/').then(|| format!("{base}{uri}"))?
        }
        None => uri.to_owned(),
    };
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::new();
    let mut index = 0;
    while let Some(&byte) = encoded.get(index) {
        if byte == b'%' {
            let hex = std::str::from_utf8(encoded.get(index + 1..index + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            bytes.push(byte);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Each result's rule and URI, sorted.
fn sarif_artifacts(log: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let mut artifacts = Vec::new();
    for result in log.pointer("/runs/0/results")?.as_array()? {
        let uri = result.pointer("/locations/0/physicalLocation/artifactLocation/uri")?;
        artifacts.push((
            result.get("ruleId")?.as_str()?.to_owned(),
            uri.as_str()?.to_owned(),
        ));
    }
    artifacts.sort();
    Some(artifacts)
}

#[test]
fn sarif_schema_check_rejects_an_invalid_log() {
    let log = serde_json::json!({
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": { "name": "cargo-adamantium" } },
            "results": [{
                "message": { "text": "broken" },
                "level": "fatal",
                "locations": [{ "physicalLocation": {
                    "artifactLocation": { "uri": "src/a file.rs" },
                } }],
            }],
        }],
    });
    let errors = sarif_schema_errors(&log).unwrap();
    assert!(
        errors.iter().any(|error| error.contains("/level")),
        "{errors:#?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("/uri")),
        "{errors:#?}"
    );
}

/// The SARIF log has one result per finding in the JSON report, at the same
/// place and for the same rule.
#[test]
fn check_reports_the_violations_fixture_as_sarif() {
    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(fixture("violations"))
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let root = PathBuf::from(report["root"].as_str().unwrap());
    let mut expected: Vec<(String, PathBuf, u64, Option<u64>)> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| {
            (
                finding["rule"].as_str().unwrap().to_owned(),
                root.join(finding["file"].as_str().unwrap()),
                finding["line"].as_u64().unwrap_or(1),
                finding["column"].as_u64(),
            )
        })
        .collect();
    expected.sort();
    assert!(expected.len() > 10, "{report}");

    let (code, log) = check_sarif(&fixture("violations"), &[], &[]).unwrap();
    assert_eq!(code, Some(1), "{log:#}");
    assert_eq!(log["version"], "2.1.0");
    let run = &log["runs"][0];
    assert_eq!(run["tool"]["driver"]["name"], "cargo-adamantium");
    assert_eq!(
        run["invocations"],
        serde_json::json!([{ "executionSuccessful": true, "toolExecutionNotifications": [] }])
    );
    let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
    let results = run["results"].as_array().unwrap();
    let mut found: Vec<(String, PathBuf, u64, Option<u64>)> = results
        .iter()
        .map(|result| {
            let rule = result["ruleId"].as_str().unwrap().to_owned();
            let index = usize::try_from(result["ruleIndex"].as_u64().unwrap()).unwrap();
            assert_eq!(rules[index]["id"], rule.as_str());
            assert_eq!(result["level"], "error");
            let region = &result["locations"][0]["physicalLocation"]["region"];
            (
                rule,
                sarif_path(run, result).unwrap(),
                region["startLine"].as_u64().unwrap(),
                region["startColumn"].as_u64(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(found, expected);
    for (_, path, _, _) in &found {
        assert!(path.is_file(), "{}", path.display());
    }
    let mut ids: Vec<&str> = rules
        .iter()
        .map(|rule| rule["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "a rule is described twice: {ids:?}");
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == "Cargo.lock" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to.join(&name))?;
        } else {
            fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

/// GitHub resolves relative URIs against the repository root, while Clippy
/// names files relative to the workspace root and the overflow check names
/// the checked package's own manifest.
#[test]
fn sarif_uris_are_relative_to_the_git_repository_root() {
    let repository = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(repository.path())
        .status()
        .unwrap();
    assert!(status.success());
    let workspace = repository.path().join("nested dir/ws");
    copy_dir(&fixture("workspace"), &workspace).unwrap();

    let (code, log) = check_sarif(&workspace.join("member"), &[], &[]).unwrap();
    assert_eq!(code, Some(1), "{log:#}");
    let base = log["runs"][0]["originalUriBaseIds"]["%SRCROOT%"]["uri"]
        .as_str()
        .unwrap();
    assert!(
        base.ends_with('/') && !base.contains("nested"),
        "the base is not the repository root: {base}"
    );
    assert_eq!(
        sarif_artifacts(&log).unwrap(),
        [
            (
                "adamantium::overflow_checks".to_owned(),
                "nested%20dir/ws/member/Cargo.toml".to_owned()
            ),
            (
                "clippy::indexing_slicing".to_owned(),
                "nested%20dir/ws/src/lib.rs".to_owned()
            ),
            (
                "clippy::string_slice".to_owned(),
                "nested%20dir/ws/shared.rs".to_owned()
            ),
            (
                "clippy::unwrap_used".to_owned(),
                "nested%20dir/ws/member/src/lib.rs".to_owned()
            ),
        ]
    );
}

/// `--manifest-path` after `--` makes Clippy check another workspace, whose
/// root its file names are relative to.
#[test]
fn sarif_uris_follow_a_forwarded_manifest_path() {
    let repository = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(repository.path())
        .status()
        .unwrap();
    assert!(status.success());
    let outer = repository.path().join("outer");
    fs::create_dir_all(outer.join("src")).unwrap();
    fs::write(outer.join("src/lib.rs"), "").unwrap();
    fs::write(
        outer.join("Cargo.toml"),
        "[package]\nname = \"outer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.release]\noverflow-checks = true\n",
    )
    .unwrap();
    copy_dir(&fixture("workspace"), &outer.join("inner")).unwrap();

    for args in [
        ["--manifest-path", "inner/Cargo.toml"].as_slice(),
        ["--manifest-path=inner/Cargo.toml"].as_slice(),
    ] {
        let (code, log) = check_sarif(&outer, &[], args).unwrap();
        assert_eq!(code, Some(1), "{args:?}: {log:#}");
        assert_eq!(
            sarif_artifacts(&log).unwrap(),
            [
                (
                    "clippy::indexing_slicing".to_owned(),
                    "outer/inner/src/lib.rs".to_owned()
                ),
                (
                    "clippy::string_slice".to_owned(),
                    "outer/inner/shared.rs".to_owned()
                ),
                (
                    "clippy::unwrap_used".to_owned(),
                    "outer/inner/member/src/lib.rs".to_owned()
                ),
            ],
            "{args:?}"
        );
    }
}

/// Outside a git repository, URIs are relative to the workspace root.
#[test]
fn sarif_uris_outside_git_are_relative_to_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    copy_dir(&fixture("workspace"), &workspace).unwrap();

    let (code, log) =
        check_sarif(&workspace, &[("GIT_CEILING_DIRECTORIES", dir.path())], &[]).unwrap();
    assert_eq!(code, Some(1), "{log:#}");
    let base = log["runs"][0]["originalUriBaseIds"]["%SRCROOT%"]["uri"]
        .as_str()
        .unwrap();
    assert!(base.ends_with("/ws/"), "{base}");
    assert_eq!(
        sarif_artifacts(&log).unwrap(),
        [
            (
                "clippy::indexing_slicing".to_owned(),
                "src/lib.rs".to_owned()
            ),
            ("clippy::string_slice".to_owned(), "shared.rs".to_owned()),
            (
                "clippy::unwrap_used".to_owned(),
                "member/src/lib.rs".to_owned()
            ),
        ]
    );
}

#[test]
fn check_reports_build_errors_as_sarif_notifications() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn broken() -> u8 { \"text\" }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.release]\noverflow-checks = true\n",
    )
    .unwrap();
    let (code, log) =
        check_sarif(dir.path(), &[("GIT_CEILING_DIRECTORIES", dir.path())], &[]).unwrap();
    assert_eq!(code, Some(2), "{log:#}");
    let run = &log["runs"][0];
    assert_eq!(run["results"], serde_json::json!([]));
    let invocation = &run["invocations"][0];
    assert_eq!(invocation["executionSuccessful"], false);
    let notifications = invocation["toolExecutionNotifications"].as_array().unwrap();
    assert!(!notifications.is_empty(), "{log:#}");
    let first = &notifications[0];
    assert_eq!(first["level"], "error");
    assert!(
        first["message"]["text"]
            .as_str()
            .unwrap()
            .contains("mismatched types"),
        "{first}"
    );
    assert_eq!(
        first["locations"][0]["physicalLocation"],
        serde_json::json!({
            "artifactLocation": { "uri": "src/lib.rs", "uriBaseId": "%SRCROOT%" },
            "region": { "startLine": 1 },
        })
    );
}

#[test]
fn check_without_a_manifest_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = adamantium().arg("check").arg(dir.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no Cargo.toml"));
}

#[test]
fn init_writes_the_rules_into_a_package_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "").unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n",
    )
    .unwrap();

    let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest = fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(
        manifest.starts_with("[package]\nname = \"demo\""),
        "{manifest}"
    );
    assert!(
        manifest.contains("[lints.rust]\nunsafe_code = \"forbid\""),
        "{manifest}"
    );
    assert!(manifest.contains("[lints.clippy]\n"), "{manifest}");
    assert!(manifest.contains("unwrap_used = \"deny\""), "{manifest}");
    for rule in [
        "unused_must_use",
        "let_underscore_future",
        "let_underscore_must_use",
    ] {
        assert!(
            manifest.contains(&format!("{rule} = \"deny\"")),
            "{manifest}"
        );
    }
    assert!(
        manifest.contains("[profile.release]\noverflow-checks = true"),
        "{manifest}"
    );
    assert!(!manifest.contains("workspace"), "{manifest}");
    let clippy = fs::read_to_string(dir.path().join("clippy.toml")).unwrap();
    assert_eq!(
        clippy,
        fs::read_to_string(repository_root().join("cli/clippy.toml")).unwrap()
    );

    let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already has the rules"));
    assert_eq!(
        fs::read_to_string(dir.path().join("Cargo.toml")).unwrap(),
        manifest
    );
}

#[test]
fn init_uses_workspace_tables_in_a_workspace() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n\n[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let manifest = fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(
        manifest.contains("[workspace.lints.rust]\nunsafe_code = \"forbid\""),
        "{manifest}"
    );
    assert!(
        manifest.contains("[workspace.lints.clippy]\n"),
        "{manifest}"
    );
    assert!(manifest.contains("[lints]\nworkspace = true"), "{manifest}");
    assert!(stderr.contains("a, b"), "{stderr}");
}

const BARE_MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";

/// A package with a bare `Cargo.toml`, whose `clippy.toml` path `write`
/// fills in.
fn package_with_clippy_toml(
    write: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("Cargo.toml"), BARE_MANIFEST)?;
    write(&dir.path().join("clippy.toml"))?;
    Ok(dir)
}

fn init(dir: &Path, force: bool) -> io::Result<Output> {
    let mut command = adamantium();
    command.arg("init");
    if force {
        command.arg("--force");
    }
    command.arg(dir).output()
}

#[test]
fn init_refuses_to_overwrite_a_different_clippy_toml_and_changes_nothing() {
    let theirs = b"msrv = \"1.88\"\n";
    let dir = package_with_clippy_toml(|path| fs::write(path, theirs)).unwrap();

    let output = init(dir.path(), false).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--force"));
    assert_eq!(
        fs::read(dir.path().join("Cargo.toml")).unwrap(),
        BARE_MANIFEST.as_bytes()
    );
    assert_eq!(fs::read(dir.path().join("clippy.toml")).unwrap(), theirs);

    let output = init(dir.path(), true).unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest = fs::read(dir.path().join("Cargo.toml")).unwrap();
    assert!(
        String::from_utf8_lossy(&manifest).contains("[lints.rust]\nunsafe_code = \"forbid\""),
        "{}",
        String::from_utf8_lossy(&manifest)
    );
    let clippy = fs::read(dir.path().join("clippy.toml")).unwrap();
    assert_eq!(
        clippy,
        fs::read(repository_root().join("cli/clippy.toml")).unwrap()
    );

    let output = init(dir.path(), false).unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already has the rules"));
    assert_eq!(fs::read(dir.path().join("Cargo.toml")).unwrap(), manifest);
    assert_eq!(fs::read(dir.path().join("clippy.toml")).unwrap(), clippy);
}

#[test]
fn init_treats_a_non_utf8_clippy_toml_as_a_conflict() {
    let theirs = b"msrv = \"\xff\"\n";
    let dir = package_with_clippy_toml(|path| fs::write(path, theirs)).unwrap();

    let output = init(dir.path(), false).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--force"));
    assert_eq!(
        fs::read(dir.path().join("Cargo.toml")).unwrap(),
        BARE_MANIFEST.as_bytes()
    );
    assert_eq!(fs::read(dir.path().join("clippy.toml")).unwrap(), theirs);

    let output = init(dir.path(), true).unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        fs::read(dir.path().join("clippy.toml")).unwrap(),
        fs::read(repository_root().join("cli/clippy.toml")).unwrap()
    );
}

#[test]
fn init_fails_before_writing_when_clippy_toml_cannot_be_read() {
    let dir = package_with_clippy_toml(|path| fs::create_dir(path)).unwrap();

    for force in [false, true] {
        let output = init(dir.path(), force).unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("reading"), "{stderr}");
        assert_eq!(
            fs::read(dir.path().join("Cargo.toml")).unwrap(),
            BARE_MANIFEST.as_bytes()
        );
        assert!(dir.path().join("clippy.toml").is_dir());
    }
}

#[test]
fn rules_match_the_repository_manifest() {
    let manifest: toml_edit::DocumentMut = fs::read_to_string(repository_root().join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    for strict in [false, true] {
        let mut command = adamantium();
        command.args(["rules", "--format", "json"]);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let rules: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut listed: Vec<(String, String, String)> = rules["lints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lint| {
                (
                    lint["tool"].as_str().unwrap().to_owned(),
                    lint["name"].as_str().unwrap().to_owned(),
                    lint["level"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        listed.sort();

        let mut tables = vec![&manifest["workspace"]["lints"]];
        if strict {
            tables.push(&manifest["workspace"]["metadata"]["adamantium-strict"]["lints"]);
        }
        let mut declared = Vec::new();
        for tables in tables {
            for tool in ["rust", "clippy"] {
                if let Some(table) = tables.get(tool).and_then(toml_edit::Item::as_table) {
                    for (name, level) in table.iter() {
                        declared.push((
                            tool.to_owned(),
                            name.to_owned(),
                            level.as_str().unwrap().to_owned(),
                        ));
                    }
                }
            }
        }
        declared.sort();
        assert_eq!(listed, declared, "strict = {strict}");
        assert_eq!(
            rules["clippy_toml"].as_str().unwrap(),
            fs::read_to_string(repository_root().join("cli/clippy.toml")).unwrap()
        );
    }
}

#[test]
fn explain_has_an_example_for_every_rule_in_text_and_json() {
    let listed = adamantium()
        .args(["rules", "--strict", "--format", "json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let rules: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let mut names: Vec<(String, String, String)> = rules["lints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lint| {
            let name = lint["name"].as_str().unwrap();
            let qualified = match lint["tool"].as_str().unwrap() {
                "rust" => name.to_owned(),
                "clippy" => format!("clippy::{name}"),
                tool => panic!("unknown tool {tool}"),
            };
            (
                name.to_owned(),
                qualified,
                lint["why"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    names.push((
        "overflow_checks".into(),
        "adamantium::overflow_checks".into(),
        "the release profile must set overflow-checks = true, so integer overflow panics instead of wrapping".into(),
    ));

    // Run outside a Cargo project: explanations must work without loading one.
    let dir = tempfile::tempdir().unwrap();
    for (name, qualified, why) in names {
        let output = adamantium()
            .current_dir(dir.path())
            .args(["adamantium", "explain", &qualified, "--format", "json"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{qualified}: {output:?}");
        assert!(output.stderr.is_empty());
        assert!(output.stdout.ends_with(b"\n"));
        let explanation: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(explanation["schema_version"], 1);
        assert_eq!(explanation["rule"], qualified);
        assert_eq!(explanation["why"], why);
        for field in ["replacement", "before", "after"] {
            assert!(
                !explanation[field].as_str().unwrap().trim().is_empty(),
                "{qualified} has no {field}"
            );
        }
        assert_ne!(explanation["before"], explanation["after"], "{qualified}");

        let bare = adamantium()
            .current_dir(dir.path())
            .args(["explain", &name, "--format", "json"])
            .output()
            .unwrap();
        assert!(bare.status.success());
        assert_eq!(bare.stdout, output.stdout);

        let text = adamantium()
            .current_dir(dir.path())
            .args(["explain", &name])
            .output()
            .unwrap();
        assert!(text.status.success());
        assert!(text.stderr.is_empty());
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.starts_with(&format!("{qualified}\n")));
        assert!(text.contains(&format!("Why: {why}")));
        for (label, field) in [
            ("Replacement: ", "replacement"),
            ("Before:\n", "before"),
            ("After:\n", "after"),
        ] {
            assert!(text.contains(&format!("{label}{}", explanation[field].as_str().unwrap())));
        }
    }
}

#[test]
fn explain_rejects_unknown_names_and_unsupported_formats() {
    for format in ["text", "json"] {
        for rule in ["not_a_rule", "rust::unwrap_used", "clippy::unsafe_code", ""] {
            let output = adamantium()
                .args(["explain", rule, "--format", format])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.contains(&format!("unknown rule `{rule}`")));
            assert!(stderr.contains("cargo adamantium rules"));
        }
    }
    for args in [
        vec!["explain"],
        vec!["explain", "unwrap_used", "--format", "toml"],
    ] {
        let output = adamantium().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn rules_as_toml_are_the_workspace_tables() {
    for strict in [false, true] {
        let mut command = adamantium();
        command.args(["rules", "--format", "toml"]);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let toml = String::from_utf8(output.stdout).unwrap();
        assert!(toml.starts_with(
            "[workspace.lints.rust]\nunsafe_code = \"forbid\"\nunused_must_use = \"deny\"\n\n[workspace.lints.clippy]\n"
        ));
        assert!(toml.contains("unwrap_used = \"deny\"\n"));
        assert_eq!(
            toml.contains("arithmetic_side_effects = \"deny\"\n"),
            strict
        );
    }
}

#[test]
fn missing_experimental_library_is_an_incomplete_check() {
    let temp = tempfile::tempdir().unwrap();
    let output = adamantium()
        .args(["check", "--format", "json", "--dylint-library"])
        .arg(temp.path().join("missing-library"))
        .arg(fixture("clean"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "error");
    assert!(report["errors"].as_array().unwrap().iter().any(|error| {
        error["message"]
            .as_str()
            .unwrap()
            .contains("experimental check incomplete")
    }));
}
