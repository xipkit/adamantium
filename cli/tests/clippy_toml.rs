//! The paths in Adamantium's `clippy.toml`. Its entries outside std carry
//! `allow-invalid`, so these tests check what that would otherwise hide: that
//! every path names a real item, and that no project gets a warning for an
//! item that its features leave out.

use std::{
    collections::BTreeSet,
    env, fs,
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use toml_edit::{Document, DocumentMut, InlineTable, Item, Value};

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
    let entries = entries(&config)?;
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
        let entry = entries
            .iter()
            .find(|(range, _)| range.start <= span.byte_start && span.byte_end <= range.end);
        reported.insert(entry.map_or(diagnostic.message, |(_, path)| path.clone()));
    }
    Ok(reported)
}

/// Where each entry of `config` is, with its path. Clippy may span a whole
/// entry or only its path, depending on its version, so a diagnostic belongs
/// to the entry whose range contains its span.
fn entries(config: &str) -> Result<Vec<(Range<usize>, String)>> {
    let document = Document::parse(config)?;
    let mut entries = Vec::new();
    for list in LISTS {
        let array = document
            .as_item()
            .get(list)
            .and_then(Item::as_array)
            .with_context(|| format!("no {list} array"))?;
        for value in array {
            let entry = value
                .as_inline_table()
                .with_context(|| format!("an entry that isn't an inline table: {value}"))?;
            let range = entry.span().context("an entry without a span")?;
            entries.push((range, path(entry)?.to_owned()));
        }
    }
    Ok(entries)
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

/// A project with only some of Tokio's features gets no warnings from the
/// shipped entries. One misspelled entry shows that Clippy loaded Tokio and
/// resolved paths in it.
#[test]
fn no_tokio_feature_set_warns() {
    for features in [
        "tokio",
        "tokio/rt",
        "tokio/rt-multi-thread",
        "tokio/sync",
        "tokio/rt,tokio/sync",
        "tokio/full",
    ] {
        let mut config = shipped().unwrap();
        let entries = config["disallowed-methods"].as_array_mut().unwrap();
        let probe = entries
            .iter()
            .filter_map(Value::as_inline_table)
            .find(|entry| path(entry).unwrap().starts_with("tokio::"))
            .map(|entry| misspelled(entry).unwrap())
            .unwrap();
        let expected = BTreeSet::from([path(&probe).unwrap().to_owned()]);
        entries.push(probe);
        assert_eq!(
            unresolved(&config, features, &[]).unwrap(),
            expected,
            "{features}"
        );
    }
}
