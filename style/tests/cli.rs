use std::{fs, process::Command};

#[test]
fn cargo_style_dry_run_is_machine_readable_without_an_api_key() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("target/generated/build.rs"), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-rustoleum"))
        .args([
            "rustoleum",
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
    assert_eq!(
        report["selected_rules"],
        serde_json::json!(["glob_import_context"])
    );
    assert_eq!(report["files"].as_array().unwrap().len(), 1);
    assert_eq!(report["files"][0]["path"], "src/main.rs");
    assert_eq!(report["files"][0]["scope"], "file");
}

#[test]
fn unknown_rule_is_an_error_without_an_api_call() {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-rustoleum"))
        .args(["check", "--dry-run", "--rule", "not_a_rule"])
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-rustoleum"));
        cmd.arg("check")
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
