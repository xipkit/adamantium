use std::{fs, process::Command};

fn cargo_adamantium_no_key() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"));
    command
        .env_remove("OPENAI_API_KEY")
        .env("ADAMANTIUM_NO_KEYCHAIN", "1");
    command
}

#[test]
fn cargo_style_dry_run_is_machine_readable_without_an_api_key() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("target/generated/build.rs"), "").unwrap();

    let output = cargo_adamantium_no_key()
        .args([
            "adamantium",
            "style",
            "check",
            "--dry-run",
            "--rule",
            "glob_import_context",
        ])
        .arg(dir.path())
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
}

#[test]
fn provider_can_be_named_and_unknown_providers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let invoke = |provider: &str| {
        cargo_adamantium_no_key()
            .args(["style", "check", "--dry-run", "--provider", provider])
            .arg(dir.path())
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
fn the_concurrency_pack_is_opt_in_and_recorded_in_the_report() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let dry_run = |args: &[&str]| {
        let output = cargo_adamantium_no_key()
            .args(["style", "check", "--dry-run"])
            .args(args)
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let google = dry_run(&[]);
    assert_eq!(google["pack"], "google");
    assert_eq!(google["selected_rules"].as_array().unwrap().len(), 51);
    let concurrency = dry_run(&["--pack", "concurrency"]);
    assert_eq!(concurrency["pack"], "concurrency");
    assert_eq!(
        concurrency["selected_rules"],
        serde_json::json!([
            "stale_read_across_await",
            "split_actor_invariant",
            "unbounded_collection_growth"
        ])
    );
    assert_ne!(concurrency["rules_sha256"], google["rules_sha256"]);

    let output = cargo_adamantium_no_key()
        .args(["style", "check", "--dry-run", "--pack", "concurrency"])
        .args(["--rule", "glob_import_context"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown rule"));

    let output = cargo_adamantium_no_key()
        .args([
            "style",
            "rules",
            "--pack",
            "concurrency",
            "--format",
            "text",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let names: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split_once(':').unwrap().0.to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "stale_read_across_await",
            "split_actor_invariant",
            "unbounded_collection_growth"
        ]
    );
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
        let mut cmd = cargo_adamantium_no_key();
        cmd.args(["style", "check"])
            .arg(dir.path())
            .args(["--rule", "glob_import_context", "--retry-from"])
            .arg(&report_path);
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
    let mut command = cargo_adamantium_no_key();
    command
        .args(["style", "check"])
        .arg(dir)
        .args(["--rule", "glob_import_context"])
        .args(args);
    command
}

#[test]
fn changed_reviews_only_files_that_differ_from_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
    fs::write(dir.path().join("kept.rs"), "fn kept() {}\n").unwrap();
    fs::write(dir.path().join("edited.rs"), "fn edited() {}\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
    fs::write(dir.path().join("edited.rs"), "fn changed() {}\n").unwrap();
    fs::write(dir.path().join("new.rs"), "fn new() {}\n").unwrap();

    let paths = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(
            &style_check(dir.path(), &["--changed", "--dry-run"])
                .output()
                .unwrap()
        ),
        ["edited.rs", "new.rs"]
    );
    assert_eq!(
        paths(
            &style_check(dir.path(), &["--changed=main", "--dry-run"])
                .output()
                .unwrap()
        ),
        ["edited.rs", "new.rs"]
    );
    assert_eq!(
        paths(&style_check(dir.path(), &["--dry-run"]).output().unwrap()),
        ["edited.rs", "kept.rs", "new.rs"]
    );

    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "--quiet", "-m", "change"]);
    let output = style_check(dir.path(), &["--changed"]).output().unwrap();
    assert!(paths(&output).is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "complete");
}

#[test]
fn a_fully_cached_run_needs_no_key_and_keeps_its_findings() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    let planned = style_check(dir.path(), &["--dry-run"]).output().unwrap();
    let mut previous: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    assert_eq!(previous["schema_version"], 3);
    assert_eq!(previous["context_sha256"], serde_json::Value::Null);
    let rules_sha256 = previous["rules_sha256"].as_str().unwrap();
    assert_eq!(rules_sha256.len(), 64);
    previous["status"] = serde_json::json!("complete");
    previous["files"][0] = serde_json::json!({
        "path": "lib.rs",
        "status": "checked",
        "cached": false,
        "source_sha256": "d015582edda04972d731a817cf2012d58db46b9951e71f55c04e52804cd9ae71",
        "findings": [{
            "rule": "glob_import_context",
            "lines": { "start_line": 1, "end_line": 1 },
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
    assert_eq!(
        report["files"][0]["findings"][0]["lines"],
        serde_json::json!({ "start_line": 1, "end_line": 1 })
    );

    // A different context is a different review, so the file is sent again,
    // which needs a key.
    let output = style_check(dir.path(), &["--cache-from", cache, "--context", "library"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("OPENAI_API_KEY"));

    // A dry run shows the cached findings but still succeeds.
    let output = style_check(dir.path(), &["--cache-from", cache, "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "planned");
    assert_eq!(report["files"][0]["cached"], true);
    assert_eq!(
        report["files"][0]["findings"][0]["rule"],
        "glob_import_context"
    );

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
fn retry_from_cannot_be_combined_with_changed_or_a_cache() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
    for other in [&["--changed"][..], &["--cache-from", "cache.json"]] {
        let mut args = vec!["--retry-from", "previous.json"];
        args.extend_from_slice(other);
        let output = style_check(dir.path(), &args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
}
