use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

#[allow(
    clippy::unwrap_used,
    reason = "a failed fixture command fails the test"
)]
fn run(args: &[&str], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .args(args)
        .arg(path)
        .output()
        .unwrap()
}

#[allow(clippy::unwrap_used, reason = "a failed fixture setup fails the test")]
fn project(source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let library = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        format!(
            r#"
[workspace]
[package]
name = "fix-fixture"
version = "0.0.0"
edition = "2024"
[dependencies]
adamantium = {{ path = {:?} }}
tokio = {{ version = "1", features = ["rt", "macros", "time"] }}
[profile.release]
overflow-checks = true
"#,
            library
        ),
    )
    .unwrap();
    fs::write(dir.path().join("src/lib.rs"), source).unwrap();
    dir
}

#[test]
fn fixes_build_and_pass_check_with_a_non_mutating_preview() {
    for source in [
        "pub fn value() -> u32 { dbg!(7) }\n",
        "pub async fn wait() { (|| async { std::hint::black_box(7); tokio::time::sleep(std::time::Duration::ZERO).await; })().await; }\n",
        "pub async fn wait() { /* café */ tokio::time::sleep(std::time::Duration::ZERO).await; }\r\n",
        "\u{feff}#!/usr/bin/env rust-script\npub async fn wait() { tokio::time::sleep(std::time::Duration::ZERO).await; }",
        "pub async fn wait() { tokio::time::sleep(std::time::Duration::ZERO).await; }\n",
        "use tokio::time::sleep as nap;\npub async fn wait() { nap(std::time::Duration::ZERO).await; }\n",
        "pub async fn work() -> usize { tokio::task::spawn_blocking(|| 7).await.unwrap() }\n",
        "use tokio::task::spawn_blocking as work;\npub async fn calculate() -> usize { work(|| 7).await.unwrap() }\n",
    ] {
        let dir = project(source);
        let file = dir.path().join("src/lib.rs");
        let preview = run(&["fix", "--dry-run"], dir.path());
        let stderr = String::from_utf8_lossy(&preview.stderr);
        assert_eq!(preview.status.code(), Some(1), "{stderr}");
        let diff = String::from_utf8(preview.stdout).unwrap();
        assert!(
            diff.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@"),
            "{diff}\n{stderr}"
        );
        assert!(diff.contains("+pub "), "{diff}");
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        let fixed = run(&["fix"], dir.path());
        assert!(
            fixed.status.success(),
            "{}",
            String::from_utf8_lossy(&fixed.stderr)
        );
        let after = fs::read_to_string(&file).unwrap();
        if source.contains("dbg!") {
            assert!(!after.contains("dbg!"), "{after}");
        } else {
            assert!(after.contains("::adamantium::"), "{after}");
        }
        let checked = run(&["check", "--format", "json"], dir.path());
        assert!(
            checked.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );
        let again = run(&["fix", "--dry-run"], dir.path());
        assert!(again.status.success());
        assert!(again.stdout.is_empty());
    }
}

#[test]
fn scope_decisions_and_non_immediate_join_handling_are_left_for_review() {
    let source = "pub async fn work() { let handle = tokio::spawn(async {}); let _ = handle.await; }\npub async fn blocking() { let result = tokio::task::spawn_blocking(|| 7).await; let _ = result; }\n";
    let dir = project(source);
    let output = run(&["fix"], dir.path());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("migrating.md#tasks"), "{stderr}");
    assert!(stderr.contains("migrating.md#blocking-work"), "{stderr}");
}

#[test]
fn skip_configuration_and_build_errors_never_edit_source() {
    let source = "pub async fn wait() { tokio::time::sleep(std::time::Duration::ZERO).await; }\n";
    let dir = project(source);
    let manifest = dir.path().join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[workspace.metadata.adamantium]\nskip = [\"src/\"]\n");
    fs::write(&manifest, text).unwrap();
    let output = run(&["fix"], dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
    fs::write(dir.path().join("src/lib.rs"), "pub fn broken(\n").unwrap();
    let output = run(&["fix"], dir.path());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn broken(\n"
    );
}

#[test]
fn ambiguous_callees_explicit_generics_and_stored_timers_are_not_rewritten() {
    for source in [
        "pub async fn wait() { ({ std::hint::black_box(7); tokio::time::sleep })(std::time::Duration::ZERO).await; }\n",
        "pub async fn wait() { let timer = tokio::time::sleep(std::time::Duration::ZERO); std::future::ready(()).await; timer.await; }\n",
        "pub async fn work() -> usize { let value = tokio::task::spawn_blocking::<_, u64>(|| 1).await.unwrap(); std::mem::size_of_val(&value) }\n",
        "pub async fn work() -> usize { ({ std::hint::black_box(7); tokio::task::spawn_blocking })(|| 1).await.unwrap() }\n",
    ] {
        let dir = project(source);
        let preview = run(&["fix", "--dry-run"], dir.path());
        assert_eq!(
            preview.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert!(
            preview.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&preview.stdout)
        );
        let output = run(&["fix"], dir.path());
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            source
        );
    }
}

#[test]
fn unavailable_or_uncertain_migration_apis_leave_source_buildable() {
    let source = "pub async fn work() -> usize { tokio::time::sleep(std::time::Duration::ZERO).await; tokio::task::spawn_blocking(|| 7).await.unwrap() }\n";
    for options in [
        ", default-features = false",
        ", optional = true",
        ", default-features = false, features = [\"tokio\"]",
    ] {
        let dir = project(source);
        let manifest = dir.path().join("Cargo.toml");
        let text = fs::read_to_string(&manifest).unwrap();
        let text = text
            .lines()
            .map(|line| {
                if line.starts_with("adamantium = ") {
                    format!("{}{options} }}", line.trim_end_matches('}').trim_end())
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(manifest, text).unwrap();
        let output = run(&["fix"], dir.path());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if options.contains("features = [") {
            assert!(output.status.success(), "{stderr}");
        } else {
            assert_eq!(output.status.code(), Some(1), "{stderr}");
            assert!(stderr.contains("migration rewrites require"), "{stderr}");
            assert_eq!(
                fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
                source
            );
        }
    }
}

#[allow(clippy::unwrap_used, reason = "a missing fixture fails the test")]
fn shared_state_fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/shared-state")
            .join(name),
    )
    .unwrap()
}

#[test]
fn shared_state_rewrites_build_pass_check_and_keep_behavior() {
    let source = shared_state_fixture("rewrite.rs");
    let dir = project(&source);
    // An actor needs no runtime, so the rewrite works without the `tokio`
    // feature.
    let manifest = dir.path().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let text = text
        .lines()
        .map(|line| {
            if line.starts_with("adamantium = ") {
                format!(
                    "{}, default-features = false }}",
                    line.trim_end_matches('}').trim_end()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(manifest, text).unwrap();
    let file = dir.path().join("src/lib.rs");

    let preview = run(&["fix", "--dry-run"], dir.path());
    assert_eq!(
        preview.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let diff = String::from_utf8(preview.stdout).unwrap();
    assert!(
        diff.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@"),
        "{diff}"
    );
    assert!(
        diff.contains("+    let hits = ::adamantium::Actor::new(0u64);"),
        "{diff}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), source);

    let fixed = run(&["fix"], dir.path());
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        shared_state_fixture("rewrite.fixed.rs")
    );
    let tested = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["test", "--quiet"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        tested.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&tested.stdout),
        String::from_utf8_lossy(&tested.stderr)
    );
    let again = run(&["fix", "--dry-run"], dir.path());
    assert!(again.status.success());
    assert!(again.stdout.is_empty());
}

#[test]
fn shared_state_fix_cannot_follow_is_reported_and_left_unchanged() {
    let source = shared_state_fixture("review.rs");
    let dir = project(&source);
    let output = run(&["fix"], dir.path());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
    for (name, section) in [
        ("held", "a-guard-held-across-statements"),
        ("awaited", "a-lock-held-across-an-await"),
        ("checking", "two-locks-at-once"),
        ("savings", "two-locks-at-once"),
        ("calls", "two-locks-at-once"),
        ("escapes", "shared-state"),
        ("macro_use", "shared-state"),
    ] {
        let line = source
            .lines()
            .position(|line| line.contains(&format!("let {name} = Arc::new(")))
            .unwrap()
            + 1;
        let report = format!("src/lib.rs:{line}: `{name}` stays an `Arc<Mutex<_>>`: ");
        let mut lines = stderr.lines().skip_while(|line| !line.starts_with(&report));
        assert!(lines.next().is_some(), "{report}\n{stderr}");
        assert!(
            lines
                .next()
                .is_some_and(|link| link.ends_with(&format!("/docs/migrating.md#{section}"))),
            "{name}: {stderr}"
        );
    }
    assert!(stderr.contains("migrating.md#rwlock"), "{stderr}");
}
