use std::{collections::BTreeMap, fs, io::Write, path::Path, process::ExitCode};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{Dependency, Report, SCHEMA_VERSION};

#[derive(Deserialize)]
pub(super) struct Baseline {
    target: String,
    dependencies: Vec<Dependency>,
}

impl Baseline {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("using {} as a baseline", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let report: serde_json::Value =
            serde_json::from_str(text).context("it is not a JSON report")?;
        let version = report
            .get("schema_version")
            .and_then(|value| value.as_u64());
        if version != Some(u64::from(SCHEMA_VERSION)) {
            bail!(
                "a dependency baseline must be a schema version {SCHEMA_VERSION} report; regenerate it with `cargo adamantium deps --format json`"
            );
        }
        let baseline: Self = serde_json::from_value(report).context("it is not a deps report")?;
        if has_gaps(&baseline.dependencies) {
            bail!("its dependency findings are incomplete because the audit has scan gaps");
        }
        Ok(baseline)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Package<'a> {
    name: &'a str,
    #[serde(flatten)]
    source: Source<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source<'a> {
    Source(&'a str),
    Path(&'a str),
}

impl<'a> Package<'a> {
    fn new(dependency: &'a Dependency) -> Result<Self> {
        let source = match (dependency.source.as_deref(), dependency.path.as_deref()) {
            (Some(source), None) if !source.is_empty() => {
                let source = if source.starts_with("git+") {
                    source.split_once('#').map_or(source, |(source, _)| source)
                } else {
                    source
                };
                Source::Source(source)
            }
            (None, Some(path)) if !path.is_empty() => Source::Path(path),
            _ => bail!(
                "missing or conflicting source for dependency {}",
                dependency.name
            ),
        };
        Ok(Self {
            name: &dependency.name,
            source,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Finding<'a> {
    Unsafe { category: &'static str },
    DetachedTask { path: &'a str, file: &'a str },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Key<'a> {
    dependency: Package<'a>,
    #[serde(flatten)]
    finding: Finding<'a>,
}

#[derive(Serialize)]
struct Change<'a> {
    #[serde(flatten)]
    key: Key<'a>,
    count: usize,
}

#[derive(Serialize)]
struct Changes<'a> {
    added: Vec<Change<'a>>,
    removed: Vec<Change<'a>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Clean,
    Findings,
    Error,
}

#[derive(Serialize)]
pub(super) struct Evaluation<'a> {
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<Changes<'a>>,
}

impl Evaluation<'_> {
    pub(super) fn exit_code(&self) -> ExitCode {
        match self.status {
            Status::Clean => ExitCode::SUCCESS,
            Status::Findings => ExitCode::from(1),
            Status::Error => ExitCode::from(2),
        }
    }

    pub(super) fn print_text(&self, out: &mut impl Write) -> std::io::Result<()> {
        let Some(changes) = &self.changes else {
            return writeln!(
                out,
                "adamantium: dependency policy could not compare findings because the audit has scan gaps"
            );
        };
        let summary = if changes.added.is_empty() {
            "no new findings"
        } else {
            "new findings"
        };
        writeln!(out, "adamantium: dependency policy: {summary}")?;
        for (label, changes) in [("added", &changes.added), ("removed", &changes.removed)] {
            for change in changes {
                let Package { name, source } = change.key.dependency;
                let (prefix, source) = match source {
                    Source::Source(source) => ("", source),
                    Source::Path(path) => ("path ", path),
                };
                write!(
                    out,
                    "  {label} {} {name} ({prefix}{source}): ",
                    change.count
                )?;
                match change.key.finding {
                    Finding::Unsafe { category } => writeln!(out, "unsafe {category}")?,
                    Finding::DetachedTask { path, file } => writeln!(out, "{path} in {file}")?,
                }
            }
        }
        Ok(())
    }
}

pub(super) fn evaluate<'a>(
    report: &'a Report,
    baseline: Option<&'a Baseline>,
) -> Result<Evaluation<'a>> {
    if let Some(baseline) = baseline
        && baseline.target != report.target
    {
        bail!(
            "baseline target {} differs from the audit target {}",
            baseline.target,
            report.target
        );
    }
    if has_gaps(&report.dependencies) {
        return Ok(Evaluation {
            status: Status::Error,
            changes: None,
        });
    }
    let before = counts(baseline.map_or(&[], |baseline| baseline.dependencies.as_slice()))
        .context("reading baseline findings")?;
    let after = counts(&report.dependencies)?;
    let added = increases(&after, &before);
    let removed = increases(&before, &after);
    Ok(Evaluation {
        status: if added.is_empty() {
            Status::Clean
        } else {
            Status::Findings
        },
        changes: Some(Changes { added, removed }),
    })
}

fn has_gaps(dependencies: &[Dependency]) -> bool {
    dependencies.iter().any(|dependency| {
        !dependency.unscanned.is_empty()
            || dependency
                .unresolved
                .keys()
                .any(|name| spawn_name(name) || matches!(name.as_str(), "local" | "unblock"))
    })
}

fn spawn_name(name: &str) -> bool {
    matches!(
        name,
        "spawn"
            | "spawn_local"
            | "spawn_blocking"
            | "spawn_on"
            | "spawn_local_on"
            | "spawn_blocking_on"
            | "spawn_many"
    )
}

fn starts_task(path: &str) -> bool {
    path.rsplit("::").next().is_some_and(spawn_name)
        || matches!(path, "async_std::task::Builder::local" | "smol::unblock")
}

fn counts(dependencies: &[Dependency]) -> Result<BTreeMap<Key<'_>, usize>> {
    let mut counts = BTreeMap::new();
    for dependency in dependencies {
        let package = Package::new(dependency)?;
        let mut add = |finding, count| -> Result<()> {
            let total = counts
                .entry(Key {
                    dependency: package,
                    finding,
                })
                .or_insert(0_usize);
            *total = total
                .checked_add(count)
                .with_context(|| format!("too many findings for {}", package.name))?;
            Ok(())
        };
        for (category, count) in dependency.unsafe_code.by_category() {
            if count > 0 {
                add(Finding::Unsafe { category }, count)?;
            }
        }
        for call in dependency
            .calls
            .iter()
            .filter(|call| starts_task(&call.path))
        {
            for site in &call.sites {
                add(
                    Finding::DetachedTask {
                        path: &call.path,
                        file: &site.file,
                    },
                    1,
                )?;
            }
        }
    }
    Ok(counts)
}

fn increases<'a>(
    current: &BTreeMap<Key<'a>, usize>,
    earlier: &BTreeMap<Key<'_>, usize>,
) -> Vec<Change<'a>> {
    current
        .iter()
        .filter_map(|(&key, &count)| {
            let increase = count.saturating_sub(earlier.get(&key).copied().unwrap_or(0));
            (increase > 0).then_some(Change {
                key,
                count: increase,
            })
        })
        .collect()
}
