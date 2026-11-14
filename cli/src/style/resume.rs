//! Reading a previous JSON report, to retry the files it could not review or
//! to reuse the results it already has.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::Path,
};

use crate::style::{
    provider::ProviderKind,
    report::{FileReport, FileStatus, Report, ReviewKey, SCHEMA_VERSION, sha256_hex},
    rules::Question,
    scan::Scan,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
struct PreviousReport {
    schema_version: u8,
    status: String,
    /// Reports written before providers were selectable came from Decisions.
    #[serde(default)]
    provider: ProviderKind,
    model: String,
    root: String,
    selected_rules: Vec<String>,
    files: Vec<PreviousFile>,
}

/// Just enough of a report to tell which schema the rest follows.
#[derive(Deserialize)]
struct SchemaVersion {
    schema_version: u8,
}

#[derive(Deserialize)]
struct PreviousFile {
    path: String,
    status: String,
}

/// Keeps only the files that `previous_path` failed to review or never sent.
/// The previous report must have the same scan root, provider, model and
/// rule names.
pub(crate) fn select_pending(
    scan: &mut Scan,
    previous_path: &Path,
    provider: ProviderKind,
    model: &str,
    questions: &[Question],
) -> Result<()> {
    let previous_file = File::open(previous_path)
        .with_context(|| format!("cannot open previous report {}", previous_path.display()))?;
    let previous: PreviousReport =
        serde_json::from_reader(previous_file).context("invalid previous JSON report")?;
    ensure!(
        matches!(previous.schema_version, 1 | SCHEMA_VERSION),
        "unsupported previous report schema"
    );
    ensure!(
        matches!(
            previous.status.as_str(),
            "planned" | "incomplete" | "complete"
        ),
        "invalid previous report status"
    );
    ensure!(
        previous.root == scan.root.to_string_lossy(),
        "previous report has a different scan root"
    );
    ensure!(
        previous.provider == provider,
        "previous report used a different provider"
    );
    ensure!(
        previous.model == model,
        "previous report used a different model"
    );
    ensure!(
        previous.selected_rules == questions.iter().map(|q| q.name.clone()).collect::<Vec<_>>(),
        "previous report used different rules; pass the same --rule options"
    );

    let mut seen = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for file in previous.files {
        ensure!(
            seen.insert(file.path.clone()),
            "previous report has a duplicate file: {}",
            file.path
        );
        match file.status.as_str() {
            "planned" | "error" | "not_checked" => {
                pending.insert(file.path);
            }
            "checked" | "incomplete" => {}
            _ => bail!(
                "previous report has an invalid file status: {}",
                file.status
            ),
        }
    }

    let discovered: BTreeSet<_> = scan
        .files
        .iter()
        .map(|path| scan.relative_path(path))
        .collect();
    if let Some(missing) = pending.difference(&discovered).next() {
        bail!("file awaiting retry is no longer in the scan: {missing}");
    }
    let root = &scan.root;
    scan.files.retain(|path| {
        path.strip_prefix(root)
            .ok()
            .is_some_and(|relative| pending.contains(relative.to_string_lossy().as_ref()))
    });
    Ok(())
}

/// Results from a previous report that can stand in for asking the provider
/// again: each was answered under the same review key, for a source whose
/// hash the report recorded.
#[derive(Default)]
pub(crate) struct Cache {
    entries: BTreeMap<String, FileReport>,
}

impl Cache {
    /// Reads the reusable results from `previous_path`. The report must have
    /// the current schema and the same scan root. If its review key differs
    /// from `key`, nothing in it is reusable and the cache is empty.
    pub fn load(previous_path: &Path, scan: &Scan, key: &ReviewKey) -> Result<Self> {
        let bytes = fs::read(previous_path)
            .with_context(|| format!("cannot open cached report {}", previous_path.display()))?;
        let version: SchemaVersion =
            serde_json::from_slice(&bytes).context("invalid cached JSON report")?;
        ensure!(
            version.schema_version == SCHEMA_VERSION,
            "a cached report needs schema version {SCHEMA_VERSION}, which records source hashes"
        );
        let previous: Report =
            serde_json::from_slice(&bytes).context("invalid cached JSON report")?;
        ensure!(
            previous.root == scan.root.to_string_lossy(),
            "cached report has a different scan root"
        );

        let mut paths = BTreeSet::new();
        for file in &previous.files {
            ensure!(
                paths.insert(file.path.as_str()),
                "cached report has a duplicate file: {}",
                file.path
            );
        }
        if previous.key != *key {
            return Ok(Self::default());
        }
        let entries = previous
            .files
            .into_iter()
            .filter(|file| {
                matches!(file.status, FileStatus::Checked | FileStatus::Incomplete)
                    && file.source_sha256.is_some()
            })
            .map(|file| (file.path.clone(), file))
            .collect();
        Ok(Self { entries })
    }

    /// Replaces each planned file in `report` whose source still has the
    /// cached hash with the cached result. `scan` must be the scan the report
    /// was planned from. A file that can't be read is left planned, so its
    /// review reports the error.
    pub fn apply(&self, report: &mut Report, scan: &Scan) {
        for (file, path) in report.files.iter_mut().zip(&scan.files) {
            if file.status != FileStatus::Planned {
                continue;
            }
            let Some(cached) = self.entries.get(&file.path) else {
                continue;
            };
            let Ok(source) = fs::read(path) else {
                continue;
            };
            if cached.source_sha256.as_deref() == Some(sha256_hex(&source).as_str()) {
                *file = FileReport {
                    cached: true,
                    input_tokens: None,
                    ..cached.clone()
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};

    use super::*;
    use crate::style::rules::RuleSet;

    fn previous_report(root: &Path, model: &str, question: &Question) -> Value {
        json!({
            "schema_version": 1,
            "status": "incomplete",
            "provider": "decisions",
            "model": model,
            "root": root,
            "selected_rules": [question.name],
            "files": [
                { "path": "a.rs", "status": "checked" },
                { "path": "b.rs", "status": "incomplete" },
                { "path": "c.rs", "status": "error" },
                { "path": "d.rs", "status": "not_checked" }
            ]
        })
    }

    #[test]
    fn retries_only_failed_or_unchecked_files_and_does_not_add_new_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "new.rs"] {
            fs::write(dir.path().join(name), "fn example() {}\n").unwrap();
        }
        let mut scan = Scan::discover(dir.path()).unwrap();
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..1];
        let report = previous_report(&scan.root, &rules.model, &questions[0]);
        let report_path = dir.path().join("report.json");
        fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        select_pending(
            &mut scan,
            &report_path,
            ProviderKind::Decisions,
            &rules.model,
            questions,
        )
        .unwrap();
        assert_eq!(
            scan.files
                .iter()
                .map(|path| scan.relative_path(path))
                .collect::<Vec<_>>(),
            ["c.rs", "d.rs"]
        );
    }

    #[test]
    fn rejects_mismatched_corrupt_or_missing_pending_inputs() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..1];
        let scan = Scan::discover(dir.path()).unwrap();
        let valid = previous_report(&scan.root, &rules.model, &questions[0]);
        let report_path = dir.path().join("report.json");
        let check = |report: &Value| {
            fs::write(&report_path, serde_json::to_vec(report).unwrap()).unwrap();
            let mut scan = Scan::discover(dir.path()).unwrap();
            select_pending(
                &mut scan,
                &report_path,
                ProviderKind::Decisions,
                &rules.model,
                questions,
            )
        };

        let mut wrong = valid.clone();
        wrong["root"] = json!("/different/root");
        assert!(check(&wrong).unwrap_err().to_string().contains("scan root"));
        wrong = valid.clone();
        wrong["provider"] = json!("claude");
        assert!(
            check(&wrong)
                .unwrap_err()
                .to_string()
                .contains("different provider")
        );
        wrong["provider"] = json!("not_a_provider");
        assert!(
            check(&wrong)
                .unwrap_err()
                .to_string()
                .contains("invalid previous JSON report")
        );
        wrong = valid.clone();
        wrong["model"] = json!("other-model");
        assert!(
            check(&wrong)
                .unwrap_err()
                .to_string()
                .contains("different model")
        );
        wrong = valid.clone();
        wrong["selected_rules"] = json!(["other_rule"]);
        assert!(
            check(&wrong)
                .unwrap_err()
                .to_string()
                .contains("different rules")
        );
        wrong = valid.clone();
        wrong["files"][1]["path"] = json!("a.rs");
        assert!(check(&wrong).unwrap_err().to_string().contains("duplicate"));
        wrong = valid.clone();
        wrong["files"][1]["status"] = json!("unknown");
        assert!(
            check(&wrong)
                .unwrap_err()
                .to_string()
                .contains("invalid file status")
        );
        fs::remove_file(dir.path().join("c.rs")).unwrap();
        assert!(
            check(&valid)
                .unwrap_err()
                .to_string()
                .contains("no longer in the scan")
        );
    }

    #[test]
    fn reports_without_a_provider_came_from_decisions() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..1];
        let mut scan = Scan::discover(dir.path()).unwrap();
        let mut report = previous_report(&scan.root, &rules.model, &questions[0]);
        report.as_object_mut().unwrap().remove("provider");
        let report_path = dir.path().join("report.json");
        fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        select_pending(
            &mut scan,
            &report_path,
            ProviderKind::Decisions,
            &rules.model,
            questions,
        )
        .unwrap();
        assert_eq!(scan.files.len(), 2);
    }

    mod cache {
        use super::*;
        use crate::style::{
            provider::Estimate,
            report::{Finding, ReportStatus},
        };

        /// A scan of `a.rs`, `b.rs`, `c.rs` and `d.rs`, each with distinct
        /// source, and the key and questions for one rule.
        fn setup() -> (tempfile::TempDir, Scan, Vec<Question>, ReviewKey) {
            let dir = tempfile::tempdir().unwrap();
            for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
                fs::write(dir.path().join(name), format!("// {name}\n")).unwrap();
            }
            let scan = Scan::discover(dir.path()).unwrap();
            let rules = RuleSet::load().unwrap();
            let questions = rules.questions[..1].to_vec();
            let key =
                ReviewKey::new(ProviderKind::Decisions, &rules.model, &questions, None).unwrap();
            (dir, scan, questions, key)
        }

        /// A report as a real run would leave it: `a.rs` with a finding,
        /// `b.rs` unresolved, `c.rs` failed and `d.rs` never sent.
        fn previous(scan: &Scan, questions: &[Question], key: &ReviewKey) -> Report {
            let mut report = Report::planned(scan, key.clone(), questions);
            let statuses = [
                FileStatus::Checked,
                FileStatus::Incomplete,
                FileStatus::Error,
                FileStatus::NotChecked,
            ];
            for ((file, path), status) in report.files.iter_mut().zip(&scan.files).zip(statuses) {
                file.status = status;
                if status != FileStatus::NotChecked {
                    file.source_sha256 = Some(sha256_hex(&fs::read(path).unwrap()));
                }
                file.input_tokens = Some(42);
            }
            report.files[0].findings.push(Finding {
                rule: questions[0].name.clone(),
                guidance: questions[0].instructions.clone(),
                estimate: Some(Estimate {
                    confidence: 0.9,
                    probabilities: BTreeMap::from([("violation".to_owned(), 0.9)]),
                }),
            });
            report.settle();
            report
        }

        fn write(dir: &Path, report: &impl serde::Serialize) -> std::path::PathBuf {
            let path = dir.join("previous.json");
            fs::write(&path, serde_json::to_vec(report).unwrap()).unwrap();
            path
        }

        fn statuses(report: &Report) -> Vec<(FileStatus, bool)> {
            report
                .files
                .iter()
                .map(|file| (file.status, file.cached))
                .collect()
        }

        #[test]
        fn reuses_answered_files_whose_source_is_unchanged() {
            let (dir, scan, questions, key) = setup();
            let path = write(dir.path(), &previous(&scan, &questions, &key));
            fs::write(dir.path().join("b.rs"), "// edited\n").unwrap();

            let mut report = Report::planned(&scan, key.clone(), &questions);
            Cache::load(&path, &scan, &key)
                .unwrap()
                .apply(&mut report, &scan);
            assert_eq!(
                statuses(&report),
                [
                    (FileStatus::Checked, true),
                    (FileStatus::Planned, false),
                    (FileStatus::Planned, false),
                    (FileStatus::Planned, false),
                ]
            );
            let cached = &report.files[0];
            assert_eq!(cached.path, "a.rs");
            assert_eq!(cached.findings.len(), 1);
            assert_eq!(cached.findings[0].rule, questions[0].name);
            assert_eq!(cached.input_tokens, None);
            assert!(report.has_planned_files());
        }

        #[test]
        fn a_cached_report_can_serve_as_the_next_cache() {
            let (dir, scan, questions, key) = setup();
            let path = write(dir.path(), &previous(&scan, &questions, &key));
            let mut report = Report::planned(&scan, key.clone(), &questions);
            Cache::load(&path, &scan, &key)
                .unwrap()
                .apply(&mut report, &scan);
            let path = write(dir.path(), &report);

            let mut next = Report::planned(&scan, key.clone(), &questions);
            Cache::load(&path, &scan, &key)
                .unwrap()
                .apply(&mut next, &scan);
            assert_eq!(statuses(&next), statuses(&report));
            assert_eq!(
                statuses(&next)[..2],
                [(FileStatus::Checked, true), (FileStatus::Incomplete, true)]
            );
            next.files.truncate(2);
            next.settle();
            assert_eq!(next.status, ReportStatus::Incomplete);
        }

        #[test]
        fn a_different_key_reuses_nothing() {
            let (dir, scan, questions, key) = setup();
            let path = write(dir.path(), &previous(&scan, &questions, &key));
            let rules = RuleSet::load().unwrap();
            let mut reworded = questions.clone();
            reworded[0].instructions.push_str(" Reworded.");
            let other_keys = [
                ReviewKey::new(ProviderKind::Decisions, "other", &questions, None).unwrap(),
                ReviewKey::new(ProviderKind::Decisions, &rules.model, &reworded, None).unwrap(),
                ReviewKey::new(
                    ProviderKind::Decisions,
                    &rules.model,
                    &questions,
                    Some("library"),
                )
                .unwrap(),
            ];
            for other in other_keys {
                let mut report = Report::planned(&scan, other.clone(), &questions);
                Cache::load(&path, &scan, &other)
                    .unwrap()
                    .apply(&mut report, &scan);
                assert!(
                    report
                        .files
                        .iter()
                        .all(|file| file.status == FileStatus::Planned && !file.cached)
                );
            }
        }

        #[test]
        fn rejects_old_corrupt_or_foreign_reports() {
            let (dir, scan, questions, key) = setup();
            let valid = serde_json::to_value(previous(&scan, &questions, &key)).unwrap();
            let load = |report: &Value| {
                let path = write(dir.path(), report);
                Cache::load(&path, &scan, &key).map(|_| ())
            };
            assert!(load(&valid).is_ok());

            let old = previous_report(&scan.root, &key.model, &questions[0]);
            assert!(
                load(&old)
                    .unwrap_err()
                    .to_string()
                    .contains("schema version 2")
            );
            let mut wrong = valid.clone();
            wrong["root"] = json!("/different/root");
            assert!(load(&wrong).unwrap_err().to_string().contains("scan root"));
            wrong = valid.clone();
            wrong["files"][1]["path"] = json!("a.rs");
            assert!(load(&wrong).unwrap_err().to_string().contains("duplicate"));
            wrong = valid.clone();
            wrong["files"][1]["status"] = json!("unknown");
            assert!(
                load(&wrong)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid cached JSON report")
            );
        }
    }
}
