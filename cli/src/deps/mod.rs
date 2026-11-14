//! `cargo adamantium deps`: which dependencies use `unsafe` code or call
//! the paths `clippy.toml` disallows, so a project can see what its
//! guarantees rest on. The rules stop at the crate boundary; this report
//! looks past it.

mod cfg;
mod disallowed;
mod metadata;
mod scan;

use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;

use self::{
    cfg::{Cfg, Target},
    disallowed::Disallowed,
    metadata::Kind,
    scan::{Crate, Site, UnsafeCounts, Unscanned},
};
use crate::manifest;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    /// A table on stdout, then the disallowed calls of each dependency.
    Text,
    /// A report on stdout.
    Json,
}

/// What the audit found in a workspace's dependencies.
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: PathBuf,
    /// The target triple the dependencies were resolved and read for.
    pub target: String,
    pub dependencies: Vec<Dependency>,
}

/// One dependency's `unsafe` code and disallowed calls.
#[derive(Serialize)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    /// Where Cargo gets it from, or null for a path dependency.
    pub source: Option<String>,
    pub kind: Kind,
    pub forbids_unsafe: bool,
    #[serde(rename = "unsafe")]
    pub unsafe_code: UnsafeCounts,
    pub calls: Vec<Call>,
    pub unresolved: BTreeMap<String, usize>,
    pub unscanned: Vec<Unscanned>,
}

/// The uses of one disallowed path.
#[derive(Serialize)]
pub struct Call {
    pub path: String,
    pub reason: String,
    pub sites: Vec<Site>,
}

/// Audits the dependencies of the workspace at `path` and prints the report.
/// `cargo_args` go to `cargo metadata` and `cargo tree`, to choose features.
pub fn run(path: &Path, format: Format, cargo_args: &[String]) -> Result<ExitCode> {
    let root = manifest::root(path)?;
    let report = audit(&root, cargo_args)?;
    let mut stdout = io::stdout().lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            writeln!(stdout)?;
        }
        Format::Text => print_text(&mut stdout, &report)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn audit(root: &Path, cargo_args: &[String]) -> Result<Report> {
    let disallowed = Disallowed::bundled()?;
    let (triple, printed) = metadata::host(root)?;
    let target = Target::parse(&printed);
    let mut dependencies = Vec::new();
    for dependency in metadata::dependencies(root, &triple, &target, cargo_args)? {
        let scan = match &dependency.lib {
            Some(lib) => scan::scan(
                &Crate {
                    name: &lib.name,
                    root: &dependency.root,
                    src: &lib.src_path,
                    edition_2015: lib.edition == "2015",
                    externs: &dependency.externs,
                    cfg: Cfg {
                        target: &target,
                        features: &dependency.features,
                        proc_macro: lib.proc_macro,
                    },
                },
                &disallowed,
            ),
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
            name: dependency.name,
            version: dependency.version,
            source: dependency.source,
            kind: dependency.kind,
            forbids_unsafe: scan.forbids_unsafe,
            unsafe_code: scan.unsafe_code,
            calls,
            unresolved: scan.unresolved,
            unscanned: scan.unscanned,
        });
    }
    Ok(Report {
        schema_version: 1,
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
