use std::{fs, process::Command};

#[test]
fn cargo_style_dry_run_is_machine_readable_without_an_api_key() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("target/generated/build.rs"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .args([
            "adamantium",
            "style",
            "check",
            "--dry-run",
            "--rule",
            "glob_import_context",
        ])
        .arg(dir.path())
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "planned");
    assert_eq!(report["provider"], "decisions");
    assert_eq!(
        report["selected_rules"],
        serde_json::json!(["glob_import_context"])
    );
    assert_eq!(report["files"].as_array().unwrap().len(), 1);
    assert_eq!(report["files"][0]["path"], "src/main.rs");
    assert_eq!(report["files"][0]["scope"], "file");
}

#[test]
fn provider_can_be_named_and_unknown_providers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let invoke = |provider: &str| {
        Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
            .args(["style", "check", "--dry-run", "--provider", provider])
            .arg(dir.path())
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap()
    };
    let output = invoke("decisions");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["provider"], "decisions");

    let output = invoke("not_a_provider");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--provider"));
}

#[test]
fn unknown_rule_is_an_error_without_an_api_call() {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .args(["style", "check", "--dry-run", "--rule", "not_a_rule"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown rule"));
}

#[test]
fn retry_uses_only_failed_and_unchecked_files_without_a_key() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "checked.rs",
        "unresolved.rs",
        "error.rs",
        "pending.rs",
        "new.rs",
    ] {
        fs::write(dir.path().join(name), "fn example() {}\n").unwrap();
    }
    let report_path = dir.path().join("previous.json");
    let mut previous = serde_json::json!({
        "schema_version": 1,
        "status": "incomplete",
        "model": "gpt-6-luna",
        "root": dir.path().canonicalize().unwrap(),
        "selected_rules": ["glob_import_context"],
        "files": [
            { "path": "checked.rs", "status": "checked" },
            { "path": "unresolved.rs", "status": "incomplete" },
            { "path": "error.rs", "status": "error" },
            { "path": "pending.rs", "status": "not_checked" }
        ]
    });
    fs::write(&report_path, serde_json::to_vec(&previous).unwrap()).unwrap();
    let invoke = |dry_run: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"));
        cmd.args(["style", "check"])
            .arg(dir.path())
            .args(["--rule", "glob_import_context", "--retry-from"])
            .arg(&report_path)
            .env_remove("OPENAI_API_KEY");
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.output().unwrap()
    };
    let output = invoke(true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths: Vec<_> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["error.rs", "pending.rs"]);

    previous["files"][2]["status"] = serde_json::json!("checked");
    previous["files"][3]["status"] = serde_json::json!("checked");
    fs::write(&report_path, serde_json::to_vec(&previous).unwrap()).unwrap();
    let output = invoke(false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["files"].as_array().unwrap().len(), 0);
}

/// `style check` on `dir` for one rule, without an API key.
fn style_check(dir: &std::path::Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"));
    command
        .args(["style", "check"])
        .arg(dir)
        .args(["--rule", "glob_import_context"])
        .args(args)
        .env_remove("OPENAI_API_KEY");
    command
}

#[test]
fn a_fully_cached_run_needs_no_key_and_keeps_its_findings() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let planned = style_check(dir.path(), &["--dry-run"]).output().unwrap();
    let mut previous: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    assert_eq!(previous["schema_version"], 2);
    assert_eq!(previous["context_sha256"], serde_json::Value::Null);
    let rules_sha256 = previous["rules_sha256"].as_str().unwrap();
    assert_eq!(rules_sha256.len(), 64);
    previous["status"] = serde_json::json!("complete");
    previous["files"][0] = serde_json::json!({
        "path": "lib.rs",
        "scope": "file",
        "status": "checked",
        "cached": false,
        "source_sha256": "d015582edda04972d731a817cf2012d58db46b9951e71f55c04e52804cd9ae71",
        "findings": [{
            "rule": "glob_import_context",
            "guidance": "Use glob imports only where they are clearer.",
            "confidence": 0.9,
            "probabilities": { "violation": 0.9 }
        }],
        "unresolved": [],
        "no_violation_count": 0,
        "input_tokens": 17,
        "error": null
    });
    let report_path = dir.path().join("previous.json");
    fs::write(&report_path, serde_json::to_vec(&previous).unwrap()).unwrap();
    let cache = report_path.to_str().unwrap();

    let output = style_check(dir.path(), &["--cache-from", cache])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["files"][0]["status"], "checked");
    assert_eq!(report["files"][0]["cached"], true);
    assert_eq!(report["files"][0]["input_tokens"], serde_json::Value::Null);
    assert_eq!(
        report["files"][0]["findings"][0]["rule"],
        "glob_import_context"
    );

    // A different context is a different review, so the file is sent again,
    // which needs a key.
    let output = style_check(dir.path(), &["--cache-from", cache, "--context", "library"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("OPENAI_API_KEY"));

    fs::write(dir.path().join("lib.rs"), "fn edited() {}\n").unwrap();
    let output = style_check(dir.path(), &["--cache-from", cache, "--dry-run"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["files"][0]["status"], "planned");
    assert_eq!(report["files"][0]["cached"], false);
}

#[test]
fn retry_from_cannot_be_combined_with_a_cache() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let output = style_check(
        dir.path(),
        &[
            "--retry-from",
            "previous.json",
            "--cache-from",
            "cache.json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}
