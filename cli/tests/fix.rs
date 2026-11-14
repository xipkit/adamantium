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
name = "fix-fixture-{}"
version = "0.0.0"
edition = "2024"
[dependencies]
adamantium = {{ path = {:?} }}
tokio = {{ version = "1", features = ["rt", "macros", "time"] }}
[profile.release]
overflow-checks = true
"#,
            dir.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .trim_start_matches('.')
                .to_ascii_lowercase(),
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
    let dir = binary(&source);
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
    let file = dir.path().join("src/main.rs");

    let preview = run(&["fix", "--dry-run"], dir.path());
    assert_eq!(
        preview.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let diff = String::from_utf8(preview.stdout).unwrap();
    assert!(
        diff.starts_with("--- a/src/main.rs\n+++ b/src/main.rs\n@@"),
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
        ("deferred", "two-locks-at-once"),
        ("pending", "shared-state"),
        ("public", "two-locks-at-once"),
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

/// A project whose only target is a binary with `main` as its main file, so
/// no other crate can call its functions.
#[allow(clippy::unwrap_used, reason = "a failed fixture setup fails the test")]
fn binary(main: &str) -> tempfile::TempDir {
    let dir = project("");
    fs::remove_file(dir.path().join("src/lib.rs")).unwrap();
    fs::write(dir.path().join("src/main.rs"), main).unwrap();
    dir
}

#[test]
fn shared_state_fields_and_parameters_build_pass_check_and_keep_behavior() {
    for name in ["fields", "params"] {
        let source = shared_state_fixture(&format!("{name}.rs"));
        let dir = binary(&source);
        let file = dir.path().join("src/main.rs");

        let fixed = run(&["fix"], dir.path());
        assert!(
            fixed.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&fixed.stderr)
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            shared_state_fixture(&format!("{name}.fixed.rs")),
            "{name}"
        );
        let tested = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["test", "--quiet"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            tested.status.success(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&tested.stdout),
            String::from_utf8_lossy(&tested.stderr)
        );
        let again = run(&["fix", "--dry-run"], dir.path());
        assert!(again.status.success(), "{name}");
        assert!(again.stdout.is_empty(), "{name}");
    }
}

#[test]
fn shared_state_fields_fix_cannot_follow_are_reported_and_left_unchanged() {
    let source = shared_state_fixture("fields-review.rs");
    let dir = binary(&source);
    let output = run(&["fix"], dir.path());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
        source
    );
    for (name, section) in [
        ("Deferred.deferred", "two-locks-at-once"),
        ("Awaited.awaited", "shared-state"),
        ("Dropped.dropped", "two-locks-at-once"),
        ("Touched.touched", "shared-state"),
        ("Printed.printed", "shared-state"),
    ] {
        let (_, field) = name.split_once('.').unwrap();
        let line = source
            .lines()
            .position(|line| line.trim_start().starts_with(&format!("{field}: Arc<")))
            .unwrap()
            + 1;
        let report = format!("src/main.rs:{line}: `{name}` stays an `Arc<Mutex<_>>`: ");
        let mut lines = stderr.lines().skip_while(|line| !line.starts_with(&report));
        assert!(lines.next().is_some(), "{report}\n{stderr}");
        assert!(
            lines
                .next()
                .is_some_and(|link| link.ends_with(&format!("/docs/migrating.md#{section}"))),
            "{name}: {stderr}"
        );
    }
}

#[test]
fn shared_state_whose_function_runs_inside_a_callback_is_left_unchanged() {
    // Rewriting `hits` would make `count` call an actor inside `total`'s
    // callback, which panics. In the second, only `bump` locks it, through
    // its parameter.
    for (source, reports) in [
        (
            "use std::sync::{Arc, Mutex};\n\nfn count() -> u32 {\n    let hits = Arc::new(Mutex::new(0u32));\n    *hits.lock().unwrap() += 1;\n    let value = *hits.lock().unwrap();\n    value\n}\n\nfn main() {\n    let total = adamantium::Actor::new(0u32);\n    total.with(|total| *total += count());\n    println!(\"{}\", total.read(|total| *total));\n}\n",
            &[(4, "two-locks-at-once")][..],
        ),
        (
            "use std::sync::{Arc, Mutex};\n\nfn count() -> u32 {\n    let hits = Arc::new(Mutex::new(0u32));\n    bump(&hits);\n    Arc::try_unwrap(hits).unwrap().into_inner().unwrap()\n}\n\nfn bump(hits: &Arc<Mutex<u32>>) {\n    *hits.lock().unwrap() += 1;\n}\n\nfn main() {\n    let total = adamantium::Actor::new(0u32);\n    total.with(|total| *total += count());\n    println!(\"{}\", total.read(|total| *total));\n}\n",
            &[(4, "shared-state"), (9, "two-locks-at-once")],
        ),
    ] {
        let dir = binary(source);
        let output = run(&["fix"], dir.path());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            source
        );
        for (line, section) in reports {
            let report = format!("src/main.rs:{line}: `hits` stays an `Arc<Mutex<_>>`: ");
            let mut lines = stderr.lines().skip_while(|line| !line.starts_with(&report));
            assert!(lines.next().is_some(), "{report}\n{stderr}");
            assert!(
                lines
                    .next()
                    .is_some_and(|link| link.ends_with(&format!("/docs/migrating.md#{section}"))),
                "{report}\n{stderr}"
            );
        }
        let ran = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "--quiet"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "1\n");
    }
}

/// Runs a fixture as a user would, including actual future suspension.
#[allow(
    clippy::unwrap_used,
    reason = "a failed fixture command fails the test"
)]
fn execute(dir: &Path) -> Vec<u8> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["run", "--quiet"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn bounded_async_fields_and_locals_keep_behavior_and_are_idempotent() {
    let source = shared_state_fixture("async.rs");
    let dir = binary(&source);
    let before = execute(dir.path());
    assert_eq!(before, b"7 11\n");
    let fixed = run(&["fix"], dir.path());
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let after = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(!after.contains("Mutex"), "{after}");
    assert!(after.contains("hits: ::adamantium::Actor<u64>"), "{after}");
    assert!(
        after.contains("let total = ::adamantium::Actor::new"),
        "{after}"
    );
    assert_eq!(execute(dir.path()), before);
    let again = run(&["fix", "--dry-run"], dir.path());
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(again.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
        after
    );
}

const ASYNC_COUNT: &str = r#"
use std::sync::{Arc, Mutex};
async fn count() -> u32 {
    let hits = Arc::new(Mutex::new(0u32));
    tokio::task::yield_now().await;
    *hits.lock().unwrap() += 1;
    let value = *hits.lock().unwrap();
    value
}
"#;

const ASYNC_FIELD_COUNT: &str = r#"
use std::sync::{Arc, Mutex};
struct Counter { hits: Arc<Mutex<u32>> }
impl Counter {
    async fn field_count(&self) -> u32 {
        let copy = self.hits.clone();
        tokio::task::yield_now().await;
        *copy.lock().unwrap() += 1;
        let value = *self.hits.lock().unwrap();
        value
    }
}
async fn count() -> u32 {
    let counter = Counter { hits: Arc::new(Mutex::new(0)) };
    counter.field_count().await
}
"#;

const POLL_DRIVER: &str = r#"
fn drive<F: std::future::Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}
"#;

#[allow(clippy::unwrap_used, reason = "a missing fixture fails the test")]
fn assert_manual(dir: &Path, filename: &str, source: &str, boundary: Option<&str>) {
    let fixed = run(&["fix"], dir);
    let stderr = String::from_utf8_lossy(&fixed.stderr);
    assert_eq!(fixed.status.code(), Some(1), "{stderr}\n{source}");
    assert_eq!(
        fs::read_to_string(dir.join(filename)).unwrap(),
        source,
        "{stderr}"
    );
    if let Some(boundary) = boundary {
        assert!(stderr.contains(boundary), "{stderr}");
        assert!(stderr.contains("rewrite remains manual"), "{stderr}");
    }
}

#[test]
fn bounded_async_escaping_futures_and_callback_polls_stay_byte_identical() {
    for (name, body) in [
        (
            "stored",
            "let future = count(); let value = future.await; println!(\"{value}\");",
        ),
        (
            "boxed",
            "let value = Box::pin(count()).await; println!(\"{value}\");",
        ),
        (
            "argument",
            "let value = drive(count()); println!(\"{value}\");",
        ),
        (
            "closure",
            "let value = (|| async { count().await })().await; println!(\"{value}\");",
        ),
        (
            "pointer",
            "let callback = count; let value = callback().await; println!(\"{value}\");",
        ),
        (
            "block",
            "let future = async { count().await }; let value = future.await; println!(\"{value}\");",
        ),
        (
            "first poll inside with",
            "let future = count(); let actor = adamantium::Actor::new(()); let value = actor.with(|_| drive(future)); println!(\"{value}\");",
        ),
        (
            "first poll inside read",
            "let future = count(); let actor = adamantium::Actor::new(()); let value = actor.read(|_| drive(future)); println!(\"{value}\");",
        ),
        (
            "resumed inside",
            "let mut future = Box::pin(count()); let mut context = std::task::Context::from_waker(std::task::Waker::noop()); assert!(std::future::Future::poll(future.as_mut(), &mut context).is_pending()); let actor = adamantium::Actor::new(()); let value = actor.with(|_| drive(future)); println!(\"{value}\");",
        ),
        (
            "two planned actors",
            "let future = count(); let total = Arc::new(Mutex::new(Vec::new())); total.lock().unwrap().push(drive(future)); let value = total.lock().unwrap()[0]; println!(\"{value}\");",
        ),
    ] {
        for state in [ASYNC_COUNT, ASYNC_FIELD_COUNT] {
            let source = format!(
                "{state}\n{POLL_DRIVER}\n#[tokio::main(flavor = \"current_thread\")]\nasync fn main() {{ {body} }}\n"
            );
            let dir = binary(&source);
            assert_eq!(execute(dir.path()), b"1\n", "{name}");
            assert_manual(
                dir.path(),
                "src/main.rs",
                &source,
                Some("future boundary `count`"),
            );
            assert_eq!(execute(dir.path()), b"1\n", "{name}");
        }
    }
}

#[test]
fn bounded_async_outer_future_and_dispatch_boundaries_stay_byte_identical() {
    for (helpers, body, boundary) in [
        (
            "async fn outer() -> u32 { count().await }",
            "let future = outer(); let value = future.await; println!(\"{value}\");",
            "outer",
        ),
        (
            "fn returned() -> impl std::future::Future<Output = u32> { count() }",
            "let value = returned().await; println!(\"{value}\");",
            "count",
        ),
        (
            "trait Run { async fn invoke(&self) -> u32; } struct Runner; impl Run for Runner { async fn invoke(&self) -> u32 { count().await } }",
            "let value = Runner.invoke().await; println!(\"{value}\");",
            "invoke",
        ),
        (
            "macro_rules! invoke { () => { count().await }; }",
            "let value = invoke!(); println!(\"{value}\");",
            "count",
        ),
        (
            "fn blocking() -> u32 { tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async { count().await }) }",
            "let _callback = blocking; println!(\"1\");",
            "count",
        ),
    ] {
        for state in [ASYNC_COUNT, ASYNC_FIELD_COUNT] {
            let source = format!(
                "{state}\n{helpers}\n#[tokio::main(flavor = \"current_thread\")]\nasync fn main() {{ {body} }}\n"
            );
            let dir = binary(&source);
            assert_eq!(execute(dir.path()), b"1\n");
            assert_manual(
                dir.path(),
                "src/main.rs",
                &source,
                Some(&format!("future boundary `{boundary}`")),
            );
        }
    }
}

#[test]
fn bounded_async_library_and_callable_or_fake_roots_stay_byte_identical() {
    let library = format!("{ASYNC_COUNT}\npub async fn entry() -> u32 {{ count().await }}\n");
    let dir = project(&library);
    assert_manual(
        dir.path(),
        "src/lib.rs",
        &library,
        Some("future boundary `entry`"),
    );

    for extra in [
        "fn call_entry() { main(); }",
        "fn entry_pointer() { let _callback = main; }",
        "#[tokio::main(flavor = \"current_thread\")] async fn callable() { let value = count().await; println!(\"{value}\"); }",
    ] {
        let source = format!(
            "{ASYNC_COUNT}\n{extra}\n#[tokio::main(flavor = \"current_thread\")] async fn main() {{ let value = count().await; println!(\"{{value}}\"); }}\n"
        );
        let dir = binary(&source);
        assert_manual(dir.path(), "src/main.rs", &source, Some("future boundary"));
    }

    // A re-exported real macro is still not a verified dependency/path, nor
    // does a Tokio annotation on an ordinary function make it the entry.
    for (extra, main) in [
        (
            "mod tokio { pub use real_tokio::*; }",
            "#[tokio::main(flavor = \"current_thread\")] async fn main() { let value = count().await; println!(\"{value}\"); }",
        ),
        (
            "#[real_tokio::main(crate = \"real_tokio\", flavor = \"current_thread\")] async fn entry() { let value = count().await; println!(\"{value}\"); }",
            "fn main() { entry(); }",
        ),
    ] {
        let source = format!(
            "{}\n{extra}\n{main}\n",
            ASYNC_COUNT.replace("tokio::task", "real_tokio::task")
        );
        let dir = binary(&source);
        let manifest = dir.path().join("Cargo.toml");
        let text = fs::read_to_string(&manifest).unwrap().replace(
            "tokio = { version",
            "real_tokio = { package = \"tokio\", version",
        );
        fs::write(manifest, text).unwrap();
        assert_eq!(execute(dir.path()), b"1\n");
        assert_manual(dir.path(), "src/main.rs", &source, Some("future boundary"));
    }
}

#[test]
fn bounded_async_path_dependency_cannot_impersonate_tokio() {
    let source = format!(
        "{ASYNC_COUNT}\n#[tokio::main(flavor = \"current_thread\")] async fn main() {{ let value = count().await; println!(\"{{value}}\"); }}\n"
    );
    let dir = binary(&source);
    let fake = dir.path().join("fake-tokio");
    fs::create_dir_all(fake.join("src")).unwrap();
    fs::write(fake.join("Cargo.toml"), "[package]\nname = \"tokio\"\nversion = \"1.0.0\"\nedition = \"2024\"\n[dependencies]\nreal_tokio = { package = \"tokio\", version = \"1\", features = [\"rt\", \"macros\"] }\n").unwrap();
    fs::write(fake.join("src/lib.rs"), "pub use real_tokio::*;\n").unwrap();
    let manifest = dir.path().join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap().replace(
        "tokio = { version = \"1\", features = [\"rt\", \"macros\", \"time\"] }",
        "tokio = { path = \"fake-tokio\" }",
    );
    fs::write(manifest, text).unwrap();
    assert_eq!(execute(dir.path()), b"1\n");
    assert_manual(dir.path(), "src/main.rs", &source, Some("future boundary"));
}

#[test]
fn bounded_async_real_nested_executors_and_planned_callbacks_stay_byte_identical() {
    let nested = format!(
        r#"{ASYNC_COUNT}
fn main() {{
    let actor = adamantium::Actor::new(());
    let value = actor.with(|_| tokio::runtime::Builder::new_current_thread()
        .build().unwrap().block_on(async {{ count().await }}));
    println!("{{value}}");
}}
"#
    );
    let callable = format!(
        r#"{ASYNC_COUNT}
#[tokio::main(flavor = "current_thread")]
async fn application() -> u32 {{ count().await }}
fn main() {{
    let actor = adamantium::Actor::new(());
    let value = actor.read(|_| application());
    println!("{{value}}");
}}
"#
    );
    let interacting = r#"
use std::sync::{Arc, Mutex};
struct Left(u32);
struct Right(u32);
impl Left { fn step_left(&mut self) { if self.0 > 0 { right(self.0 - 1); } } }
impl Right { fn step_right(&mut self) { if self.0 > 0 { left(self.0 - 1); } } }
fn left(depth: u32) {
    let a = Arc::new(Mutex::new(Left(depth)));
    a.lock().unwrap().step_left();
}
fn right(depth: u32) {
    let b = Arc::new(Mutex::new(Right(depth)));
    b.lock().unwrap().step_right();
}
async fn outer() { left(1); }
#[tokio::main(flavor = "current_thread")]
async fn main() { outer().await; println!("1"); }
"#;
    for source in [&nested, &callable, interacting] {
        let dir = binary(source);
        assert_eq!(execute(dir.path()), b"1\n");
        assert_manual(dir.path(), "src/main.rs", source, None);
        assert_eq!(execute(dir.path()), b"1\n");
    }
}

#[test]
fn bounded_async_one_unproved_block_refuses_every_connected_use() {
    for state in [ASYNC_COUNT, ASYNC_FIELD_COUNT] {
        let source = state
            .replace(
                "*hits.lock().unwrap() += 1;",
                "let future = async { *hits.lock().unwrap() += 1; }; future.await;",
            )
            .replace(
                "*copy.lock().unwrap() += 1;",
                "let future = async { *copy.lock().unwrap() += 1; }; future.await;",
            );
        let source = format!(
            "{source}\n#[tokio::main(flavor = \"current_thread\")] async fn main() {{ let value = count().await; println!(\"{{value}}\"); }}\n"
        );
        let dir = binary(&source);
        assert_eq!(execute(dir.path()), b"1\n");
        assert_manual(
            dir.path(),
            "src/main.rs",
            &source,
            Some("future boundary `async block`"),
        );
    }
}

#[test]
fn bounded_async_cargo_target_identity_is_required() {
    let source = shared_state_fixture("async.rs");
    for library_only in [false, true] {
        let dir = if library_only {
            project(&source)
        } else {
            binary(&source)
        };
        if !library_only {
            fs::write(dir.path().join("src/lib.rs"), "pub fn unrelated() {}\n").unwrap();
        }
        let filename = if library_only {
            "src/lib.rs"
        } else {
            "src/main.rs"
        };
        assert_manual(dir.path(), filename, &source, None);
    }

    // The entry is identified by Cargo, not by a conventional filename.
    let dir = binary(&source);
    fs::rename(
        dir.path().join("src/main.rs"),
        dir.path().join("src/entry.rs"),
    )
    .unwrap();
    let manifest = dir.path().join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[[bin]]\nname = \"custom-entry\"\npath = \"src/entry.rs\"\n");
    fs::write(manifest, text).unwrap();
    assert_eq!(execute(dir.path()), b"7 11\n");
    let fixed = run(&["fix"], dir.path());
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    assert!(
        !fs::read_to_string(dir.path().join("src/entry.rs"))
            .unwrap()
            .contains("Mutex")
    );
    assert_eq!(execute(dir.path()), b"7 11\n");
}
