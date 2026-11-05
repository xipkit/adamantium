//! The paths in Adamantium's `clippy.toml`. Its entries outside std carry
//! `allow-invalid`, so these tests check what that would otherwise hide: that
//! every path names a real item, and that no project gets a warning for an
//! item that its features leave out.

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use toml_edit::{DocumentMut, InlineTable, Value};

/// The arrays of `clippy.toml` whose entries name paths.
const LISTS: [&str; 2] = ["disallowed-types", "disallowed-methods"];

/// Every feature that defines an item `clippy.toml` names. `tokio::task::Builder`
/// also needs `--cfg tokio_unstable`.
const ALL_FEATURES: &str = "futures,parking_lot,tokio/full,tokio/tracing";

/// A line of `cargo --message-format=json`, with the fields these tests read.
#[derive(Deserialize)]
struct CargoMessage {
    message: Option<Diagnostic>,
}

#[derive(Deserialize)]
struct Diagnostic {
    message: String,
    spans: Vec<Span>,
}

#[derive(Deserialize)]
struct Span {
    file_name: PathBuf,
    byte_start: usize,
    byte_end: usize,
    is_primary: bool,
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn shipped() -> Result<DocumentMut> {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../clippy.toml"))?;
    Ok(text.parse()?)
}

fn path(entry: &InlineTable) -> Result<&str> {
    entry
        .get("path")
        .and_then(Value::as_str)
        .with_context(|| format!("an entry without a path: {entry}"))
}

/// A copy of `entry` whose path names nothing, and which Clippy warns about.
fn misspelled(entry: &InlineTable) -> Result<InlineTable> {
    let mut copy = entry.clone();
    copy.insert("path", format!("{}_misspelled", path(entry)?).into());
    copy.remove("allow-invalid");
    Ok(copy)
}

/// Runs Clippy on the `paths` fixture with `config` as its `clippy.toml`, and
/// returns what it reports about the configuration: the path of each entry it
/// couldn't resolve, or the message of any other diagnostic.
fn unresolved(
    config: &DocumentMut,
    features: &str,
    rustflags: &[&str],
) -> Result<BTreeSet<String>> {
    let config = config.to_string();
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("clippy.toml");
    fs::write(&file, &config)?;
    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(fixture("paths"))
        .env("CLIPPY_CONF_DIR", dir.path())
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"))
        .args(["clippy", "--quiet", "--message-format=json"])
        .args(["--features", features])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut reported = BTreeSet::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let message: CargoMessage = serde_json::from_str(line)?;
        let Some(diagnostic) = message.message else {
            continue;
        };
        let Some(span) = diagnostic.spans.iter().find(|span| span.is_primary) else {
            continue;
        };
        if span.file_name != file {
            continue;
        }
        let entry = config
            .get(span.byte_start..span.byte_end)
            .and_then(|text| text.parse::<Value>().ok())
            .and_then(|entry| {
                entry
                    .as_inline_table()
                    .and_then(|entry| entry.get("path"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        reported.insert(entry.unwrap_or(diagnostic.message));
    }
    Ok(reported)
}

/// Strips `allow-invalid` from every entry and adds a misspelled copy of each,
/// then resolves them all with every feature on. Clippy must report exactly
/// the copies: every real path resolves, and a misspelling of any of them
/// would be caught.
#[test]
fn every_path_resolves_with_every_feature_on() {
    let mut config = shipped().unwrap();
    let mut expected = BTreeSet::new();
    for list in LISTS {
        let entries = config[list].as_array_mut().unwrap();
        let mut copies = Vec::new();
        for entry in entries.iter_mut() {
            let entry = entry.as_inline_table_mut().unwrap();
            entry.remove("allow-invalid");
            let copy = misspelled(entry).unwrap();
            expected.insert(path(&copy).unwrap().to_owned());
            copies.push(copy);
        }
        entries.extend(copies);
    }
    let reported = unresolved(&config, ALL_FEATURES, &["--cfg", "tokio_unstable"]).unwrap();
    let wrong: Vec<_> = reported.difference(&expected).collect();
    assert!(wrong.is_empty(), "these don't resolve: {wrong:?}");
    let unchecked: Vec<_> = expected.difference(&reported).collect();
    assert!(
        unchecked.is_empty(),
        "Clippy didn't check these, so the fixture may not use their crate: {unchecked:?}"
    );
}
