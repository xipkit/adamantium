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
