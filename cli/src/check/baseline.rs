use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use super::{SCHEMA_VERSION, Status};

#[derive(Debug)]
pub(super) struct Baseline {
    original: String,
    report: Value,
    remaining: BTreeMap<BaselineKey, VecDeque<usize>>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BaselineKey {
    rule: String,
    path: String,
    source: Option<String>,
}

#[derive(Deserialize)]
struct EarlierReport {
    root: PathBuf,
    status: Status,
    findings: Vec<EarlierFinding>,
}

#[derive(Deserialize)]
struct EarlierFinding {
    rule: String,
    file: String,
    source: Option<String>,
}

impl Baseline {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(text).with_context(|| format!("using {} as a baseline", path.display()))
    }

    pub(super) fn parse(original: String) -> Result<Self> {
        let report: Value = serde_json::from_str(&original).context("it is not a JSON report")?;
        let version = report.get("schema_version").and_then(Value::as_u64);
        if version != Some(u64::from(SCHEMA_VERSION)) {
            let found = version.map_or_else(
                || "no schema version".to_owned(),
                |version| format!("schema version {version}"),
            );
            bail!(
                "it has {found}, but a baseline must be a schema version {SCHEMA_VERSION} report; regenerate it with `cargo adamantium check --format json`"
            );
        }
        let earlier = EarlierReport::deserialize(&report).context("it is not a check report")?;
        if earlier.status == Status::Error {
            bail!("the project did not build when it was written, so its findings are incomplete");
        }
        let mut remaining = BTreeMap::<_, VecDeque<_>>::new();
        for (index, finding) in earlier.findings.into_iter().enumerate() {
            let key = BaselineKey {
                path: portable_path(&earlier.root, &finding.file),
                rule: finding.rule,
                source: finding.source,
            };
            remaining.entry(key).or_default().push_back(index);
        }
        Ok(Self {
            original,
            report,
            remaining,
        })
    }

    pub(super) fn take(&mut self, rule: &str, path: &str, source: Option<&str>) -> bool {
        let key = BaselineKey {
            rule: rule.to_owned(),
            path: path.to_owned(),
            source: source.map(str::to_owned),
        };
        self.remaining
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
            .is_some()
    }

    pub(super) fn unmatched(&self) -> usize {
        self.remaining.values().map(VecDeque::len).sum()
    }

    pub(super) fn prune(mut self, path: &Path) -> Result<usize> {
        let stale: HashSet<usize> = self.remaining.values().flatten().copied().collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let findings = self
            .report
            .get_mut("findings")
            .and_then(Value::as_array_mut)
            .context("the baseline report has no findings array")?;
        let mut index = 0;
        findings.retain(|_| {
            let matched = !stale.contains(&index);
            index += 1;
            matched
        });
        if findings.is_empty() {
            *self
                .report
                .get_mut("status")
                .context("the baseline report has no status")? =
                serde_json::to_value(Status::Clean)?;
        }
        replace_baseline(path, &self.original, &self.report)
            .with_context(|| format!("updating baseline {}", path.display()))?;
        Ok(stale.len())
    }
}

fn replace_baseline(path: &Path, original: &str, report: &Value) -> Result<()> {
    let path = path.canonicalize()?;
    let parent = path
        .parent()
        .context("the baseline file has no parent directory")?;
    let mut updated = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut updated, report)?;
    writeln!(updated)?;

    #[cfg(unix)]
    let _lock = {
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        file.lock()?;
        file
    };
    updated
        .as_file()
        .set_permissions(fs::metadata(&path)?.permissions())?;
    updated.as_file().sync_all()?;
    if fs::read_to_string(&path)? != original {
        bail!("the baseline changed during the check; rerun the check to update it");
    }
    updated.persist(&path)?;
    Ok(())
}

pub(super) fn portable_path(root: &Path, file: &str) -> String {
    let root = root.to_string_lossy().replace('\\', "/");
    let file = file.replace('\\', "/");
    file.strip_prefix(root.trim_end_matches('/'))
        .and_then(|rest| rest.strip_prefix('/'))
        .map_or_else(|| file.clone(), str::to_owned)
}
