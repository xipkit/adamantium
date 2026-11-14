//! `cargo adamantium deps` on the `deps` fixture, whose path dependencies
//! stand in for real crates. In their sources, each use of a disallowed path
//! ends with a comment naming the entry it matches, and each call the audit
//! can only count by name ends with `// unresolved`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/deps")
}

fn deps(args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .arg("deps")
        .arg(fixture())
        .args(args)
        .output()
}

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Uses of disallowed paths, as `(entry, file, line)`.
type Sites = BTreeSet<(String, String, u64)>;

fn report(args: &[&str]) -> TestResult<Value> {
    let output = deps(&[&["--format", "json"], args].concat())?;
    if output.status.code() != Some(0) {
        return Err(String::from_utf8_lossy(&output.stderr).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn dependency<'a>(report: &'a Value, name: &str) -> TestResult<&'a Value> {
    report["dependencies"]
        .as_array()
        .and_then(|dependencies| {
            dependencies
                .iter()
                .find(|dependency| dependency["name"] == name)
        })
        .ok_or_else(|| format!("no {name} in {report}").into())
}

/// Every Rust file under `dir`, relative to `root`.
fn sources(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            sources(root, &path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let name = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push((name, fs::read_to_string(&path)?));
        }
    }
    Ok(())
}

/// The sites the comments in a vendored crate's sources mark, as
/// `(entry, file, line)`, and the number of lines marked `unresolved`.
fn marked(name: &str) -> io::Result<(Sites, usize)> {
    let root = fixture().join("vendor").join(name);
    let mut files = Vec::new();
    sources(&root, &root.join("src"), &mut files)?;
    let mut sites = BTreeSet::new();
    let mut unresolved = 0;
    for (file, text) in files {
        for (index, line) in text.lines().enumerate() {
            let Some((_, comment)) = line.rsplit_once("// ") else {
                continue;
            };
            if comment == "unresolved" {
                unresolved += 1;
            } else if comment.contains("::") && !comment.contains(' ') {
                let line = u64::try_from(index + 1).map_err(io::Error::other)?;
                sites.insert((comment.to_owned(), file.clone(), line));
            }
        }
    }
    Ok((sites, unresolved))
}

/// The sites a dependency's report lists. Every call has a reason.
fn reported(dependency: &Value) -> TestResult<Sites> {
    let mut sites = BTreeSet::new();
    let field = |value: &Value, name: &str| -> TestResult<String> {
        value[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("no {name} in {value}").into())
    };
    for call in dependency["calls"].as_array().ok_or("no calls")? {
        if field(call, "reason")?.is_empty() {
            return Err(format!("no reason for {call}").into());
        }
        for site in call["sites"].as_array().ok_or("no sites")? {
            let line = site["line"].as_u64().ok_or("no line")?;
            sites.insert((field(call, "path")?, field(site, "file")?, line));
        }
    }
    Ok(sites)
}

#[test]
fn deps_lists_runtime_and_build_dependencies_but_not_members_or_dev_dependencies() {
    let report = report(&[]).unwrap();
    assert_eq!(report["schema_version"], 1);
    let kinds: BTreeMap<&str, &str> = report["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| {
            (
                dependency["name"].as_str().unwrap(),
                dependency["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        BTreeMap::from([
            ("builder", "build"),
            ("codegen", "build"),
            ("helper", "build"),
            ("old", "runtime"),
            ("pool", "runtime"),
            ("safe", "runtime"),
            ("spawner", "runtime"),
            ("tokio", "runtime"),
        ])
    );
}

#[test]
fn deps_finds_each_marked_call_and_nothing_else() {
    let report = report(&[]).unwrap();
    for name in [
        "spawner", "tokio", "old", "builder", "pool", "safe", "helper",
    ] {
        let (sites, unresolved) = marked(name).unwrap();
        let dependency = dependency(&report, name).unwrap();
        assert_eq!(reported(dependency).unwrap(), sites, "{name}: {dependency}");
        let counted: u64 = dependency["unresolved"]
            .as_object()
            .unwrap()
            .values()
            .map(|count| count.as_u64().unwrap())
            .sum();
        assert_eq!(
            counted,
            u64::try_from(unresolved).unwrap(),
            "{name}: {dependency}"
        );
    }
    assert_eq!(
        dependency(&report, "spawner").unwrap()["unresolved"],
        json!({"spawn": 5})
    );
}

#[test]
fn deps_counts_each_kind_of_unsafe_and_notes_forbidding_crates() {
    let report = report(&[]).unwrap();
    let spawner = dependency(&report, "spawner").unwrap();
    assert_eq!(
        spawner["unsafe"],
        json!({"functions": 1, "blocks": 1, "impls": 1, "traits": 1, "in_macros": 1})
    );
    assert_eq!(spawner["forbids_unsafe"], false);
    assert_eq!(
        dependency(&report, "tokio").unwrap()["unsafe"]["functions"],
        1
    );
    assert_eq!(
        dependency(&report, "helper").unwrap()["unsafe"]["blocks"],
        1
    );
    // By attribute, by a lint level string, and by a lint level table.
    for name in ["safe", "old", "codegen"] {
        assert_eq!(
            dependency(&report, name).unwrap()["forbids_unsafe"],
            true,
            "{name}"
        );
    }
}

#[test]
fn deps_names_source_it_could_not_scan() {
    let report = report(&[]).unwrap();
    assert_eq!(
        dependency(&report, "spawner").unwrap()["unscanned"],
        json!([{
            "file": "src/lib.rs",
            "line": 9,
            "reason": "module `missing` has no file: neither missing.rs nor missing/mod.rs",
        }])
    );
    assert_eq!(
        dependency(&report, "tokio").unwrap()["unscanned"],
        json!([])
    );
}

#[test]
fn deps_passes_features_to_cargo_metadata() {
    let off = ("tokio::spawn".to_owned(), "src/off.rs".to_owned(), 3);
    let without = report(&[]).unwrap();
    assert!(
        !reported(dependency(&without, "spawner").unwrap())
            .unwrap()
            .contains(&off)
    );
    let with = report(&["--", "--features", "off"]).unwrap();
    assert!(
        reported(dependency(&with, "spawner").unwrap())
            .unwrap()
            .contains(&off)
    );
}

#[test]
fn deps_prints_a_table_and_each_call_as_text() {
    let output = deps(&[]).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    for line in [
        "      5     15           5  spawner 0.1.0 (partly unscanned)",
        "      0      0           0  safe 0.0.0 (forbids unsafe)",
        "      1      0           0  helper 0.0.0",
        "  tokio::task::spawn: an unstructured task; spawn inside adamantium::Tasks",
        "    src/nested/deeper.rs:2:5",
        "  unresolved calls: spawn 5",
        "  unscanned: src/lib.rs:9: module `missing` has no file: neither missing.rs nor missing/mod.rs",
        "adamantium: 8 dependencies; 3 with unsafe code, 4 with disallowed calls, 1 with unresolved calls, 1 partly unscanned",
    ] {
        assert!(
            stdout.lines().any(|printed| printed == line),
            "{line:?} in\n{stdout}"
        );
    }
    let runtime = stdout.find("\nRuntime\n").unwrap();
    let build = stdout.find("\nBuild time").unwrap();
    let helper = stdout.find("helper 0.0.0").unwrap();
    assert!(runtime < build && build < helper, "{stdout}");
}

#[test]
fn deps_without_a_manifest_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .arg("deps")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no Cargo.toml"));
}
