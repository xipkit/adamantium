use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
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
fn check_reports_every_rule_the_violations_fixture_breaks() {
    let output = adamantium()
        .args(["adamantium", "check", "--format", "json"])
        .arg(fixture("violations"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
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
    let first = &report["findings"][0];
    assert!(first["file"].as_str().unwrap().ends_with(".rs"));
    assert!(first["line"].as_u64().is_some());
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
    assert!(
        manifest.contains("[profile.release]\noverflow-checks = true"),
        "{manifest}"
    );
    assert!(!manifest.contains("workspace"), "{manifest}");
    let clippy = fs::read_to_string(dir.path().join("clippy.toml")).unwrap();
    assert_eq!(
        clippy,
        fs::read_to_string(repository_root().join("clippy.toml")).unwrap()
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

#[test]
fn init_refuses_to_overwrite_a_different_clippy_toml() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("clippy.toml"), "msrv = \"1.85\"\n").unwrap();
    let output = adamantium().arg("init").arg(dir.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--force"));
    let output = adamantium()
        .args(["init", "--force"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(
        fs::read_to_string(dir.path().join("clippy.toml"))
            .unwrap()
            .contains("disallowed-types")
    );
}

#[test]
fn rules_match_the_repository_manifest() {
    let output = adamantium()
        .args(["rules", "--format", "json"])
        .output()
        .unwrap();
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

    let manifest: toml_edit::DocumentMut = fs::read_to_string(repository_root().join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let mut declared = Vec::new();
    for tool in ["rust", "clippy"] {
        let table = manifest["workspace"]["lints"][tool].as_table().unwrap();
        for (name, level) in table.iter() {
            declared.push((
                tool.to_owned(),
                name.to_owned(),
                level.as_str().unwrap().to_owned(),
            ));
        }
    }
    declared.sort();
    assert_eq!(listed, declared);
    assert_eq!(
        rules["clippy_toml"].as_str().unwrap(),
        fs::read_to_string(repository_root().join("clippy.toml")).unwrap()
    );
}

#[test]
fn rules_as_toml_are_the_workspace_tables() {
    let output = adamantium()
        .args(["rules", "--format", "toml"])
        .output()
        .unwrap();
    let toml = String::from_utf8(output.stdout).unwrap();
    assert!(toml.starts_with(
        "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n\n[workspace.lints.clippy]\n"
    ));
    assert!(toml.contains("unwrap_used = \"deny\"\n"));
}
