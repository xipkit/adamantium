//! The paths in Adamantium's `clippy.toml`. Its external-crate entries carry
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
const ALL_FEATURES: &str = "async-channel,async-std/unstable,crossbeam-channel,flume,futures,parking_lot,smol,tokio/full,tokio/tracing";

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
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .args(["rules", "--strict", "--format", "json"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rules: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(rules
        .get("clippy_toml")
        .and_then(serde_json::Value::as_str)
        .context("the rules contain no Clippy configuration")?
        .parse()?)
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
    // Clippy names the file by its canonical path, which differs when the
    // temporary directory is reached through a symlink, as on macOS.
    let file = dir.path().canonicalize()?.join("clippy.toml");
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

/// Strips `allow-invalid` from every crate path and adds a misspelled copy of
/// each, then resolves them all with every feature on. Clippy must report
/// exactly the copies. Slice methods are checked separately because Clippy
/// suppresses unresolved-path warnings when the root isn't a loaded crate.
#[test]
fn every_crate_path_resolves_with_every_feature_on() {
    let mut config = shipped().unwrap();
    let mut expected = BTreeSet::new();
    for list in LISTS {
        let entries = config[list].as_array_mut().unwrap();
        let mut copies = Vec::new();
        for entry in entries.iter_mut() {
            let entry = entry.as_inline_table_mut().unwrap();
            entry.remove("allow-invalid");
            if path(entry).unwrap().starts_with("slice::") {
                continue;
            }
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

/// Rust resolves slice methods directly, including a misspelling that Clippy
/// would silently ignore. Build the references from the shipped entries so
/// every configured slice method is checked.
#[test]
fn every_slice_method_resolves() {
    let config = shipped().unwrap();
    let mut source = String::from("fn check() {\n");
    for entry in config["disallowed-methods"].as_array().unwrap() {
        let path = path(entry.as_inline_table().unwrap()).unwrap();
        if let Some(method) = path.strip_prefix("slice::") {
            source.push_str(&format!("    let _ = <[u8]>::{method};\n"));
        }
    }
    source.push_str("}\n");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("methods.rs");
    fs::write(&file, source).unwrap();
    let output = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--crate-type=lib", "--emit=metadata"])
        .arg(&file)
        .arg("-o")
        .arg(dir.path().join("methods.rmeta"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The paths fixture uses Tokio without dependencies that can re-enable its
/// optional features. One misspelled entry shows that Clippy loaded Tokio and
/// resolved paths in it even when sync is off.
#[test]
fn no_tokio_feature_set_warns() {
    let shipped = shipped().unwrap();
    for features in [
        "tokio",
        "tokio/rt",
        "tokio/rt-multi-thread",
        "tokio/sync",
        "tokio/rt,tokio/sync",
        "tokio/full",
    ] {
        let mut config = shipped.clone();
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
