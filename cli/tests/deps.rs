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
    deps_at(&fixture(), args)
}

fn deps_at(path: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
        .arg("deps")
        .arg(path)
        .args(args)
        .output()
}

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

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

fn marked(name: &str) -> io::Result<(Sites, usize)> {
    let root = fixture().join("vendor").join(name);
    let mut files = Vec::new();
    sources(&root, &root, &mut files)?;
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
    assert_eq!(report["schema_version"], 2);
    assert!(report.get("policy").is_none());
    assert_eq!(dependency(&report, "safe").unwrap()["path"], "vendor/safe");
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
            ("assembly", "runtime"),
            ("builder", "build"),
            ("cfg-if", "runtime"),
            ("codegen", "build"),
            ("exports", "runtime"),
            ("facade", "runtime"),
            ("ffi", "runtime"),
            ("futures", "runtime"),
            ("futures-util", "runtime"),
            ("helper", "build"),
            ("old", "runtime"),
            ("platform", "runtime"),
            ("pool", "runtime"),
            ("runner", "runtime"),
            ("safe", "runtime"),
            ("selector", "runtime"),
            ("spawner", "runtime"),
            ("tokio", "runtime"),
        ])
    );
}

#[test]
fn deps_finds_each_marked_call_and_nothing_else() {
    let report = report(&[]).unwrap();
    for name in [
        "spawner", "tokio", "old", "runner", "builder", "pool", "safe", "helper", "platform",
        "facade", "selector",
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
        json!({"leak": 1, "spawn": 5})
    );
}

#[test]
fn deps_counts_each_kind_of_unsafe_and_notes_forbidding_crates() {
    let report = report(&[]).unwrap();
    let spawner = dependency(&report, "spawner").unwrap();
    assert_eq!(
        spawner["unsafe"],
        json!({
            "functions": 1,
            "blocks": 1,
            "impls": 1,
            "traits": 1,
            "extern_blocks": 0,
            "attributes": 0,
            "global_asm": 0,
            "in_macros": 1,
        })
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
    assert_eq!(
        dependency(&report, "ffi").unwrap()["unsafe"],
        json!({
            "functions": 0,
            "blocks": 0,
            "impls": 0,
            "traits": 0,
            "extern_blocks": 1,
            "attributes": 0,
            "global_asm": 0,
            "in_macros": 0,
        })
    );
    assert_eq!(
        dependency(&report, "exports").unwrap()["unsafe"],
        json!({
            "functions": 0,
            "blocks": 0,
            "impls": 0,
            "traits": 0,
            "extern_blocks": 0,
            "attributes": 9,
            "global_asm": 0,
            "in_macros": 4,
        })
    );
    assert_eq!(
        dependency(&report, "assembly").unwrap()["unsafe"],
        json!({
            "functions": 0,
            "blocks": 0,
            "impls": 0,
            "traits": 0,
            "extern_blocks": 0,
            "attributes": 0,
            "global_asm": 13,
            "in_macros": 2,
        })
    );
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
            "line": 11,
            "reason": "module `missing` has no file: neither missing.rs nor missing/mod.rs",
        }])
    );
    assert_eq!(
        dependency(&report, "tokio").unwrap()["unscanned"],
        json!([])
    );
}

#[test]
fn deps_names_modules_declared_in_macro_bodies_it_could_not_read() {
    let report = report(&[]).unwrap();
    assert_eq!(
        dependency(&report, "platform").unwrap()["unscanned"],
        json!([{
            "file": "src/lib.rs",
            "line": 22,
            "reason": "module `hidden` is declared in a macro body the scan can't read",
        }])
    );
}

#[test]
fn deps_scans_each_conditional_macro_branch_that_may_be_built() {
    let report = report(&[]).unwrap();
    assert_eq!(
        dependency(&report, "platform").unwrap()["unsafe"],
        json!({
            "functions": 4,
            "blocks": 1,
            "impls": 1,
            "traits": 1,
            "extern_blocks": 0,
            "attributes": 0,
            "global_asm": 0,
            "in_macros": 0,
        })
    );
}

#[test]
fn deps_reads_modules_the_crates_own_macros_declare() {
    let report = report(&[]).unwrap();
    let facade = dependency(&report, "facade").unwrap();
    assert_eq!(
        facade["unsafe"],
        json!({
            "functions": 1,
            "blocks": 1,
            "impls": 1,
            "traits": 0,
            "extern_blocks": 0,
            "attributes": 0,
            "global_asm": 0,
            "in_macros": 0,
        })
    );
    assert_eq!(facade["unscanned"], json!([]));
    let files: BTreeSet<String> = reported(facade)
        .unwrap()
        .into_iter()
        .map(|(_, file, _)| file)
        .collect();
    assert_eq!(
        files,
        BTreeSet::from(["src/driver.rs", "src/macros.rs"].map(str::to_owned))
    );
}

#[test]
fn deps_reads_modules_other_crates_macros_declare_where_called_by_path_or_use() {
    let report = report(&[]).unwrap();
    let runner = dependency(&report, "runner").unwrap();
    assert_eq!(runner["unscanned"], json!([]));
    let files: BTreeSet<String> = reported(runner)
        .unwrap()
        .into_iter()
        .map(|(_, file, _)| file)
        .collect();
    assert_eq!(
        files,
        BTreeSet::from(
            [
                "src/fast.rs",
                "src/slow.rs",
                "src/imported/fast.rs",
                "src/imported/slow.rs"
            ]
            .map(str::to_owned)
        )
    );
}

#[test]
fn deps_reads_modules_reexported_macros_from_other_crates_declare() {
    let report = report(&[]).unwrap();
    let selector = dependency(&report, "selector").unwrap();
    assert_eq!(selector["unscanned"], json!([]));
    let files: BTreeSet<String> = reported(selector)
        .unwrap()
        .into_iter()
        .map(|(_, file, _)| file)
        .collect();
    assert_eq!(
        files,
        BTreeSet::from(["src/first.rs", "src/second.rs"].map(str::to_owned))
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
        "      5     30           6  spawner 0.1.0 (partly unscanned)",
        "      0      0           0  safe 0.0.0 (forbids unsafe)",
        "      1      0           0  helper 0.0.0",
        "      1      0           0  ffi 0.0.0",
        "     13      0           0  exports 0.0.0",
        "     15      0           0  assembly 0.0.0",
        "      7      2           0  platform 0.0.0 (partly unscanned)",
        "  tokio::task::spawn: an unstructured task; spawn inside adamantium::Tasks",
        "    src/nested/deeper.rs:2:5",
        "  unresolved calls: leak 1, spawn 5",
        "  unscanned: src/lib.rs:11: module `missing` has no file: neither missing.rs nor missing/mod.rs",
        "  unscanned: src/lib.rs:22: module `hidden` is declared in a macro body the scan can't read",
        "      3      2           0  facade 0.0.0",
        "    src/driver.rs:2:5",
        "    src/macros.rs:15:13",
        "adamantium: 18 dependencies; 8 with unsafe code, 8 with disallowed calls, 1 with unresolved calls, 2 partly unscanned",
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

struct PolicyFixture {
    dir: tempfile::TempDir,
    baseline: PathBuf,
}

impl PolicyFixture {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("src"))?;
        fs::create_dir_all(dir.path().join("vendor/probe/src"))?;
        fs::write(dir.path().join("src/lib.rs"), "")?;
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = 'app'\nversion = '0.1.0'\nedition = '2024'\n\
             [dependencies]\nprobe = { path = 'vendor/probe' }\n",
        )?;
        let baseline = dir.path().join("deps-baseline.json");
        Ok(Self { dir, baseline })
    }

    fn install(&self, version: &str, source: &str) -> TestResult<()> {
        let probe = self.dir.path().join("vendor/probe");
        fs::write(
            probe.join("Cargo.toml"),
            format!("[package]\nname = 'probe'\nversion = '{version}'\nedition = '2024'\n"),
        )?;
        fs::write(probe.join("src/lib.rs"), source)?;
        let mut cargo = Command::new(env!("CARGO"));
        if self.dir.path().join("Cargo.lock").exists() {
            cargo.args(["update", "-p", "probe"]);
        } else {
            cargo.arg("generate-lockfile");
        }
        let output = cargo
            .arg("--offline")
            .current_dir(self.dir.path())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn audit(&self, args: &[&str]) -> io::Result<Output> {
        deps_at(
            self.dir.path(),
            &[args, &["--", "--locked", "--offline"]].concat(),
        )
    }

    fn save_baseline(&self) -> TestResult<()> {
        let output = self.audit(&["--format", "json"])?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(&self.baseline, output.stdout)?;
        Ok(())
    }

    fn compare(&self, format: &str) -> io::Result<Output> {
        let baseline = self
            .baseline
            .to_str()
            .ok_or_else(|| io::Error::other("the test baseline path is not UTF-8"))?;
        self.audit(&["--format", format, "--deny-new", "--baseline", baseline])
    }
}

fn policy_report(output: &Output, code: i32) -> TestResult<Value> {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn deps_policy_uses_the_manifest_baseline_unless_the_cli_overrides_it() -> TestResult<()> {
    let fixture = PolicyFixture::new()?;
    let caller = tempfile::tempdir()?;
    let manifest = fixture.dir.path().join("Cargo.toml");
    let ci = fixture.dir.path().join("ci");
    fs::create_dir(&ci)?;
    let original = fs::read_to_string(&manifest)?;
    fs::write(
        &manifest,
        format!(
            "{original}\n[workspace]\nexclude = ['vendor']\n\
             [workspace.metadata.adamantium.deps]\nbaseline = 'ci/deps-baseline.json'\n"
        ),
    )?;

    fixture.install("0.1.0", "pub fn safe() {}\n")?;
    fixture.save_baseline()?;
    fs::copy(&fixture.baseline, caller.path().join("override.json"))?;
    fixture.install("0.2.0", "pub unsafe fn probe() {}\n")?;
    fixture.save_baseline()?;
    let baseline = ci.join("deps-baseline.json");
    fs::rename(&fixture.baseline, &baseline)?;

    let audit = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cargo-adamantium"))
            .current_dir(caller.path())
            .args(["adamantium", "deps", "--format", "json"])
            .arg(&manifest)
            .args(args)
            .args(["--", "--locked", "--offline"])
            .output()
    };

    let configured = policy_report(&audit(&["--deny-new"])?, 0)?;
    assert_eq!(configured["policy"]["status"], "clean");
    let overridden = policy_report(&audit(&["--deny-new", "--baseline", "override.json"])?, 1)?;
    assert_eq!(overridden["policy"]["status"], "findings");

    fs::remove_file(baseline)?;
    let output = audit(&["--deny-new"])?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("deps-baseline.json"));
    let report = policy_report(&audit(&[])?, 0)?;
    assert!(report.get("policy").is_none());
    Ok(())
}

#[test]
fn deps_policy_compares_an_unchanged_lockfile_and_dependency_updates() -> TestResult<()> {
    let fixture = PolicyFixture::new()?;
    fixture.install(
        "0.1.0",
        "pub unsafe fn first() {}\npub fn work() { std::thread::spawn(|| {}); }\n",
    )?;
    let lockfile = fixture.dir.path().join("Cargo.lock");
    let initial_lock = fs::read(&lockfile)?;
    fixture.save_baseline()?;

    let unchanged = policy_report(&fixture.compare("json")?, 0)?;
    assert_eq!(
        unchanged["policy"],
        json!({
            "status": "clean",
            "changes": { "added": [], "removed": [] },
        })
    );
    assert_eq!(fs::read(&lockfile)?, initial_lock);

    let from_zero = policy_report(&fixture.audit(&["--format", "json", "--deny-new"])?, 1)?;
    let original_findings = &from_zero["policy"]["changes"]["added"];
    assert_eq!(
        original_findings,
        &json!([
            {
                "dependency": { "name": "probe", "path": "vendor/probe" },
                "kind": "unsafe", "category": "functions", "count": 1
            },
            {
                "dependency": { "name": "probe", "path": "vendor/probe" },
                "kind": "detached_task", "path": "std::thread::spawn", "file": "src/lib.rs", "count": 1
            },
        ])
    );

    fixture.install(
        "0.2.0",
        "
        pub unsafe fn first() {}
        pub unsafe fn second() {}
        pub fn work() {
            std::thread::spawn(|| {});
            std::thread::spawn(|| {});
            std::thread::sleep(std::time::Duration::ZERO);
        }",
    )?;
    assert_ne!(fs::read(&lockfile)?, initial_lock);
    let updated = policy_report(&fixture.compare("json")?, 1)?;
    let probe = dependency(&updated, "probe")?;
    assert_eq!(probe["version"], "0.2.0");
    assert_eq!(probe["unsafe"]["functions"], 2);
    assert_eq!(probe["calls"].as_array().unwrap().len(), 2);
    assert_eq!(updated["policy"]["changes"]["added"], *original_findings);
    assert_eq!(updated["policy"]["changes"]["removed"], json!([]));
    let text = fixture.compare("text")?;
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8(text.stdout)?;
    assert!(text.contains("added 1 probe (path vendor/probe): unsafe functions"));
    assert!(text.contains("added 1 probe (path vendor/probe): std::thread::spawn in src/lib.rs"));

    fixture.install(
        "0.3.0",
        "pub fn work() { std::thread::sleep(std::time::Duration::ZERO); }",
    )?;
    let reduced = policy_report(&fixture.compare("json")?, 0)?;
    assert_eq!(reduced["policy"]["changes"]["added"], json!([]));
    assert_eq!(reduced["policy"]["changes"]["removed"], *original_findings);
    Ok(())
}

#[test]
fn deps_policy_rejects_an_incomplete_scan_and_an_incomplete_baseline() -> TestResult<()> {
    let fixture = PolicyFixture::new()?;
    fixture.install("0.1.0", "")?;
    fixture.save_baseline()?;
    fixture.install("0.2.0", "mod missing;\npub unsafe fn new_finding() {}")?;

    let partial = policy_report(&fixture.compare("json")?, 2)?;
    assert_eq!(partial["policy"], json!({"status": "error"}));
    let probe = dependency(&partial, "probe")?;
    assert_eq!(probe["unsafe"]["functions"], 1);
    assert!(
        probe["unscanned"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("module `missing`")
    );
    fixture.save_baseline()?;
    fixture.install("0.3.0", "")?;
    let output = fixture.compare("json")?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)?.contains("its dependency findings are incomplete"));
    Ok(())
}

#[test]
fn deps_policy_distinguishes_path_sources_with_the_same_name() -> TestResult<()> {
    let fixture = PolicyFixture::new()?;
    fixture.install("0.1.0", "pub unsafe fn first() {}")?;
    fixture.save_baseline()?;

    let root = fixture.dir.path();
    let manifest = root.join("Cargo.toml");
    let updated = fs::read_to_string(&manifest)?.replace("vendor/probe", "vendor/other");
    fs::rename(root.join("vendor/probe"), root.join("vendor/other"))?;
    fs::write(&manifest, updated)?;

    let report = policy_report(&fixture.compare("json")?, 1)?;
    let changes = &report["policy"]["changes"];
    assert_eq!(
        changes["added"][0]["dependency"],
        json!({"name": "probe", "path": "vendor/other"})
    );
    assert_eq!(
        changes["removed"][0]["dependency"],
        json!({"name": "probe", "path": "vendor/probe"})
    );
    Ok(())
}

#[test]
fn deps_policy_treats_unresolved_task_calls_as_scan_gaps() -> TestResult<()> {
    let fixture = PolicyFixture::new()?;
    fixture.install("0.1.0", "pub fn work() { Vec::new().insert(0, 1); }")?;
    let unrelated = policy_report(&fixture.audit(&["--format", "json", "--deny-new"])?, 0)?;
    assert_eq!(
        dependency(&unrelated, "probe")?["unresolved"],
        json!({"insert": 1})
    );

    fixture.install(
        "0.2.0",
        "pub fn work() { std::thread::Builder::new().spawn(|| {}); }",
    )?;
    let partial = policy_report(&fixture.audit(&["--format", "json", "--deny-new"])?, 2)?;
    assert_eq!(partial["policy"], json!({"status": "error"}));
    assert_eq!(
        dependency(&partial, "probe")?["unresolved"],
        json!({"spawn": 1})
    );
    Ok(())
}
