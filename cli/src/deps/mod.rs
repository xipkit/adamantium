mod cfg;
mod disallowed;
pub(crate) mod metadata;
mod policy;
mod rules;
mod scan;

use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use self::{
    cfg::{Cfg, Target},
    disallowed::Disallowed,
    metadata::Kind,
    scan::{Crate, Site, UnsafeCounts, Unscanned},
};
use crate::manifest;

const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    #[value(help = "A table on stdout, then the disallowed calls of each dependency")]
    Text,
    #[value(help = "A report on stdout")]
    Json,
}

/// What the audit found in a workspace's dependencies.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    pub target: String,
    pub dependencies: Vec<Dependency>,
}

/// One dependency's `unsafe` code and disallowed calls.
#[derive(Deserialize, Serialize)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub kind: Kind,
    pub forbids_unsafe: bool,
    #[serde(rename = "unsafe")]
    pub unsafe_code: UnsafeCounts,
    pub calls: Vec<Call>,
    pub unresolved: BTreeMap<String, usize>,
    pub unscanned: Vec<Unscanned>,
}

/// The uses of one disallowed path.
#[derive(Deserialize, Serialize)]
pub struct Call {
    pub path: String,
    pub reason: String,
    pub sites: Vec<Site>,
}

#[derive(Serialize)]
struct Output<'a> {
    #[serde(flatten)]
    report: &'a Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<policy::Evaluation<'a>>,
}

/// Audits the dependencies of the workspace at `path` and prints the report.
/// `cargo_args` go to `cargo metadata` and `cargo tree`, to choose features.
/// With `deny_new`, the exit status indicates regressions or an incomplete audit.
pub fn run(
    path: &Path,
    format: Format,
    cargo_args: &[String],
    deny_new: bool,
    baseline: Option<&Path>,
) -> Result<ExitCode> {
    let baseline = baseline.map(policy::Baseline::load).transpose()?;
    let root = manifest::root(path)?;
    let report = audit(&root, cargo_args)?;
    let policy = deny_new
        .then(|| policy::evaluate(&report, baseline.as_ref()))
        .transpose()?;
    let exit = policy
        .as_ref()
        .map_or(ExitCode::SUCCESS, policy::Evaluation::exit_code);
    let mut stdout = io::stdout().lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(
                &mut stdout,
                &Output {
                    report: &report,
                    policy,
                },
            )?;
            writeln!(stdout)?;
        }
        Format::Text => {
            print_text(&mut stdout, &report)?;
            if let Some(policy) = policy {
                policy.print_text(&mut stdout)?;
            }
        }
    }
    Ok(exit)
}

fn audit(root: &Path, cargo_args: &[String]) -> Result<Report> {
    let disallowed = Disallowed::bundled()?;
    let (triple, printed) = metadata::host(root)?;
    let target = Target::parse(&printed);
    let resolved = metadata::dependencies(root, &triple, &target, cargo_args)?;
    let externs: HashMap<&str, BTreeMap<String, String>> = resolved
        .iter()
        .map(|dependency| {
            let names = dependency
                .externs
                .iter()
                .map(|(name, dependency)| (name.clone(), dependency.lib.clone()))
                .collect();
            (dependency.id.as_str(), names)
        })
        .collect();
    let indices: HashMap<&str, usize> = resolved
        .iter()
        .filter(|dependency| dependency.lib.is_some())
        .enumerate()
        .map(|(index, dependency)| (dependency.id.as_str(), index))
        .collect();
    let crates: Vec<Crate> = resolved
        .iter()
        .filter_map(|dependency| {
            let lib = dependency.lib.as_ref()?;
            let externs = externs.get(dependency.id.as_str())?;
            let dependencies = dependency
                .externs
                .iter()
                .filter_map(|(name, dep)| Some((name.clone(), *indices.get(dep.id.as_str())?)))
                .collect();
            Some(Crate {
                name: &lib.name,
                root: &dependency.root,
                src: &lib.src_path,
                edition_2015: lib.edition == "2015",
                externs,
                dependencies,
                cfg: Cfg {
                    target: &target,
                    features: &dependency.features,
                    proc_macro: lib.proc_macro,
                },
            })
        })
        .collect();
    let mut dependencies = Vec::new();
    for dependency in &resolved {
        let scan = match indices.get(dependency.id.as_str()).copied() {
            Some(entry) => scan::scan(&crates, entry, &disallowed),
            None => scan::Scan {
                unscanned: vec![Unscanned {
                    file: "Cargo.toml".to_owned(),
                    line: None,
                    reason: "the package has no library target".to_owned(),
                }],
                ..scan::Scan::default()
            },
        };
        let calls = scan
            .calls
            .into_iter()
            .filter_map(|(index, mut sites)| {
                let entry = disallowed.entries.get(index)?;
                sites.sort();
                Some(Call {
                    path: entry.path.clone(),
                    reason: entry.reason.clone(),
                    sites,
                })
            })
            .collect();
        dependencies.push(Dependency {
            name: dependency.name.clone(),
            version: dependency.version.clone(),
            source: dependency.source.clone(),
            path: dependency.source.is_none().then(|| {
                let dir = dependency
                    .root
                    .strip_prefix(root)
                    .unwrap_or(&dependency.root);
                let path = dir.to_string_lossy();
                if path.is_empty() {
                    ".".to_owned()
                } else if cfg!(windows) {
                    path.replace('\\', "/")
                } else {
                    path.into_owned()
                }
            }),
            kind: dependency.kind,
            forbids_unsafe: scan.forbids_unsafe,
            unsafe_code: scan.unsafe_code,
            calls,
            unresolved: scan.unresolved,
            unscanned: scan.unscanned,
        });
    }
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        root: root.to_path_buf(),
        target: triple,
        dependencies,
    })
}

fn print_text(out: &mut impl Write, report: &Report) -> io::Result<()> {
    writeln!(
        out,
        "Dependencies of {} for {}",
        report.root.display(),
        report.target
    )?;
    for (kind, title) in [
        (Kind::Runtime, "Runtime"),
        (
            Kind::Build,
            "Build time: build dependencies and procedural macros",
        ),
    ] {
        let group: Vec<&Dependency> = report
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == kind)
            .collect();
        if group.is_empty() {
            continue;
        }
        writeln!(out, "\n{title}\n")?;
        writeln!(
            out,
            "{:>7} {:>6} {:>11}  crate",
            "unsafe", "calls", "unresolved"
        )?;
        for dependency in group {
            let calls: usize = dependency.calls.iter().map(|call| call.sites.len()).sum();
            let unresolved: usize = dependency.unresolved.values().sum();
            write!(
                out,
                "{:>7} {:>6} {:>11}  {} {}",
                dependency.unsafe_code.total(),
                calls,
                unresolved,
                dependency.name,
                dependency.version
            )?;
            if dependency.forbids_unsafe {
                write!(out, " (forbids unsafe)")?;
            }
            if !dependency.unscanned.is_empty() {
                write!(out, " (partly unscanned)")?;
            }
            writeln!(out)?;
        }
    }

    for dependency in &report.dependencies {
        if dependency.calls.is_empty()
            && dependency.unresolved.is_empty()
            && dependency.unscanned.is_empty()
        {
            continue;
        }
        writeln!(out, "\n{} {}", dependency.name, dependency.version)?;
        for call in &dependency.calls {
            writeln!(out, "  {}: {}", call.path, call.reason)?;
            for site in &call.sites {
                writeln!(out, "    {}:{}:{}", site.file, site.line, site.column)?;
            }
        }
        if !dependency.unresolved.is_empty() {
            let names: Vec<String> = dependency
                .unresolved
                .iter()
                .map(|(name, count)| format!("{name} {count}"))
                .collect();
            writeln!(out, "  unresolved calls: {}", names.join(", "))?;
        }
        for unscanned in &dependency.unscanned {
            match unscanned.line {
                Some(line) => writeln!(
                    out,
                    "  unscanned: {}:{line}: {}",
                    unscanned.file, unscanned.reason
                )?,
                None => writeln!(out, "  unscanned: {}: {}", unscanned.file, unscanned.reason)?,
            }
        }
    }

    let count = |test: fn(&Dependency) -> bool| {
        report
            .dependencies
            .iter()
            .filter(|dependency| test(dependency))
            .count()
    };
    writeln!(
        out,
        "\nadamantium: {} dependencies; {} with unsafe code, {} with disallowed calls, {} with unresolved calls, {} partly unscanned",
        report.dependencies.len(),
        count(|dependency| dependency.unsafe_code.total() > 0),
        count(|dependency| !dependency.calls.is_empty()),
        count(|dependency| !dependency.unresolved.is_empty()),
        count(|dependency| !dependency.unscanned.is_empty()),
    )
}
