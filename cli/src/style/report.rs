use std::{fmt, fs::File, io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::style::{
    provider::{
        Answer, Estimate, Evaluation, FileReview, Provider, ProviderKind, ReviewError, Verdict,
    },
    rules::Question,
    scan::Scan,
};

/// The version of the JSON report this build writes.
pub(crate) const SCHEMA_VERSION: u8 = 2;

#[derive(Deserialize, Serialize)]
pub(crate) struct Report {
    pub schema_version: u8,
    pub status: ReportStatus,
    #[serde(flatten)]
    pub key: ReviewKey,
    pub root: String,
    pub selected_rules: Vec<String>,
    pub files: Vec<FileReport>,
}

/// Everything besides a file's path and source that decides its review. Two
/// reviews of the same source under the same key ask the same questions.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewKey {
    pub provider: ProviderKind,
    pub model: String,
    /// SHA-256 of the selected rules' full definitions, as sent.
    pub rules_sha256: String,
    /// SHA-256 of `--context`, or `None` without one.
    pub context_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportStatus {
    Planned,
    Incomplete,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct FileReport {
    pub path: String,
    pub scope: Scope,
    pub status: FileStatus,
    /// Whether these results were carried over from a previous report
    /// instead of asking the provider again.
    pub cached: bool,
    /// SHA-256 of the source the results describe.
    pub source_sha256: Option<String>,
    pub findings: Vec<Finding>,
    pub unresolved: Vec<Unresolved>,
    pub no_violation_count: usize,
    /// Input tokens spent on this file in this run; `None` for cached files.
    pub input_tokens: Option<u64>,
    pub error: Option<String>,
}

/// The part of a file a finding applies to. Providers give no reliable
/// locations, so findings describe the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileStatus {
    /// Not sent yet, or not sent because this is a dry run.
    Planned,
    /// Every rule was answered.
    Checked,
    /// The provider answered, but left some rules unresolved.
    Incomplete,
    /// The file could not be reviewed.
    Error,
    /// Not sent because an earlier service failure stopped the run.
    NotChecked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Finding {
    pub rule: String,
    pub guidance: String,
    /// `confidence` and `probabilities`, written only by providers that give
    /// them.
    #[serde(flatten)]
    pub estimate: Option<Estimate>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Unresolved {
    pub rule: String,
    pub reason: UnresolvedReason,
    /// `confidence` and `probabilities`, written only for an insufficient
    /// context answer from a provider that gives them.
    #[serde(flatten)]
    pub estimate: Option<Estimate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnresolvedReason {
    Refusal,
    InsufficientContext,
}

impl ReviewKey {
    pub fn new(
        provider: ProviderKind,
        model: &str,
        questions: &[Question],
        context: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            provider,
            model: model.to_owned(),
            rules_sha256: sha256_hex(&serde_json::to_vec(questions)?),
            context_sha256: context.map(|context| sha256_hex(context.as_bytes())),
        })
    }
}

/// The lowercase hexadecimal SHA-256 digest of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Report {
    /// A report listing every scanned file as planned.
    pub fn planned(scan: &Scan, key: ReviewKey, questions: &[Question]) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: ReportStatus::Planned,
            key,
            root: scan.root.to_string_lossy().into_owned(),
            selected_rules: questions
                .iter()
                .map(|question| question.name.clone())
                .collect(),
            files: scan
                .files
                .iter()
                .map(|path| FileReport::new(scan.relative_path(path)))
                .collect(),
        }
    }

    /// Whether any file is still waiting to be sent to the provider.
    pub fn has_planned_files(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.status == FileStatus::Planned)
    }

    /// Sends each planned file to the provider, in scan order, then settles
    /// the report's status. `scan` must be the scan the report was planned
    /// from.
    pub fn check(
        &mut self,
        scan: &Scan,
        questions: &[Question],
        context: Option<&str>,
        provider: &dyn Provider,
    ) {
        let mut service_failed = false;
        for (file, path) in self.files.iter_mut().zip(&scan.files) {
            if file.status != FileStatus::Planned {
                continue;
            }
            if service_failed {
                file.status = FileStatus::NotChecked;
                continue;
            }
            let source = match read_source(path, provider.max_source_bytes()) {
                Ok(source) => source,
                Err(error) => {
                    file.fail(format!("cannot read source: {error}"));
                    continue;
                }
            };
            file.source_sha256 = Some(sha256_hex(source.as_bytes()));
            let review = FileReview {
                model: &self.key.model,
                questions,
                path: &file.path,
                source: &source,
                context,
            };
            let outcome = provider.review(&review);
            match outcome {
                Ok(evaluation) => {
                    if let Err(error) = file.record(questions, evaluation) {
                        file.fail(format!("{error:#}"));
                        service_failed = true;
                    }
                }
                Err(ReviewError::Input(error)) => file.fail(format!("{error:#}")),
                Err(ReviewError::Service(error)) => {
                    file.fail(format!("{error:#}"));
                    service_failed = true;
                }
            }
        }
        self.settle();
    }

    /// Sets the status of a report whose files have all been reviewed or
    /// taken from a cache.
    pub fn settle(&mut self) {
        self.status = if self
            .files
            .iter()
            .all(|file| file.status == FileStatus::Checked)
        {
            ReportStatus::Complete
        } else {
            ReportStatus::Incomplete
        };
    }

    /// A planned report exits 0 even when cached files carry findings: a dry
    /// run previews the review, it doesn't judge it.
    pub fn exit_code(&self) -> u8 {
        match self.status {
            ReportStatus::Planned => 0,
            ReportStatus::Incomplete => 2,
            ReportStatus::Complete if self.files.iter().any(|file| !file.findings.is_empty()) => 1,
            ReportStatus::Complete => 0,
        }
    }
}

fn read_source(path: &Path, limit: usize) -> Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit)?.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "file exceeds the provider's input limit"
    );
    String::from_utf8(bytes).context("source is not UTF-8")
}

impl FileReport {
    fn new(path: String) -> Self {
        Self {
            path,
            scope: Scope::File,
            status: FileStatus::Planned,
            cached: false,
            source_sha256: None,
            findings: Vec::new(),
            unresolved: Vec::new(),
            no_violation_count: 0,
            input_tokens: None,
            error: None,
        }
    }

    fn fail(&mut self, error: String) {
        self.status = FileStatus::Error;
        self.error = Some(error);
    }

    fn record(&mut self, questions: &[Question], evaluation: Evaluation) -> Result<()> {
        ensure!(
            evaluation.answers.len() == questions.len(),
            "provider returned {} answers for {} questions",
            evaluation.answers.len(),
            questions.len()
        );
        for (question, answer) in questions.iter().zip(evaluation.answers) {
            let rule = question.name.clone();
            match answer {
                Answer::Refusal => self.unresolved.push(Unresolved {
                    rule,
                    reason: UnresolvedReason::Refusal,
                    estimate: None,
                }),
                Answer::Choice {
                    verdict: Verdict::Violation,
                    estimate,
                } => self.findings.push(Finding {
                    rule,
                    guidance: question.instructions.clone(),
                    estimate,
                }),
                Answer::Choice {
                    verdict: Verdict::NoViolation,
                    ..
                } => self.no_violation_count += 1,
                Answer::Choice {
                    verdict: Verdict::InsufficientContext,
                    estimate,
                } => self.unresolved.push(Unresolved {
                    rule,
                    reason: UnresolvedReason::InsufficientContext,
                    estimate,
                }),
            }
        }
        self.status = if self.unresolved.is_empty() {
            FileStatus::Checked
        } else {
            FileStatus::Incomplete
        };
        self.input_tokens = evaluation.input_tokens;
        Ok(())
    }
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Planned => "planned",
            Self::Checked => "checked",
            Self::Incomplete => "incomplete",
            Self::Error => "error",
            Self::NotChecked => "not_checked",
        })
    }
}

impl fmt::Display for ReportStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Planned => "planned",
            Self::Incomplete => "incomplete",
            Self::Complete => "complete",
        })
    }
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Refusal => "refusal",
            Self::InsufficientContext => "insufficient_context",
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
    };

    use adamantium::Actor;
    use anyhow::anyhow;

    use super::*;
    use crate::style::rules::RuleSet;

    /// Answers every file with the same verdicts, or fails as configured.
    struct FakeProvider {
        answers: Option<Vec<Verdict>>,
        estimate: Option<Estimate>,
        refuse_input: bool,
        reviews: Actor<Vec<(String, String, Option<String>)>>,
    }

    impl FakeProvider {
        fn answering(verdicts: &[Verdict]) -> Self {
            Self {
                answers: Some(verdicts.to_vec()),
                estimate: Some(Estimate {
                    confidence: 0.8,
                    probabilities: BTreeMap::from([
                        ("violation".to_owned(), 0.7),
                        ("no_violation".to_owned(), 0.2),
                        ("insufficient_context".to_owned(), 0.1),
                    ]),
                }),
                refuse_input: false,
                reviews: Actor::default(),
            }
        }

        fn unavailable() -> Self {
            Self {
                answers: None,
                estimate: None,
                refuse_input: false,
                reviews: Actor::default(),
            }
        }

        fn review_count(&self) -> usize {
            self.reviews.with(|reviews| reviews.len())
        }
    }

    impl Provider for FakeProvider {
        fn max_source_bytes(&self) -> usize {
            64
        }

        fn review(&self, file: &FileReview<'_>) -> Result<Evaluation, ReviewError> {
            self.reviews.with(|reviews| {
                reviews.push((
                    file.path.to_owned(),
                    file.source.to_owned(),
                    file.context.map(str::to_owned),
                ));
            });
            if self.refuse_input {
                return Err(ReviewError::Input(anyhow!("too large for this model")));
            }
            let verdicts = self
                .answers
                .clone()
                .ok_or_else(|| ReviewError::Service(anyhow!("service unavailable")))?;
            Ok(Evaluation {
                answers: verdicts
                    .into_iter()
                    .map(|verdict| Answer::Choice {
                        verdict,
                        estimate: self.estimate.clone(),
                    })
                    .collect(),
                input_tokens: Some(123),
            })
        }
    }

    fn check(dir: &Path, questions: &[Question], provider: &FakeProvider) -> Report {
        let scan = Scan::discover(dir).unwrap();
        let key = ReviewKey::new(
            ProviderKind::Decisions,
            "test-model",
            questions,
            Some("library"),
        )
        .unwrap();
        let mut report = Report::planned(&scan, key, questions);
        report.check(&scan, questions, Some("library"), provider);
        report
    }

    #[test]
    fn reports_file_scoped_findings_and_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..2];
        let fake = FakeProvider::answering(&[Verdict::Violation, Verdict::InsufficientContext]);
        let report = check(dir.path(), questions, &fake);
        assert_eq!(report.status, ReportStatus::Incomplete);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.key.provider, ProviderKind::Decisions);
        assert_eq!(report.key.model, "test-model");
        assert_eq!(report.files[0].scope, Scope::File);
        assert_eq!(report.files[0].path, "lib.rs");
        assert_eq!(report.files[0].findings[0].rule, questions[0].name);
        assert_eq!(
            report.files[0].findings[0].guidance,
            questions[0].instructions
        );
        assert_eq!(report.files[0].unresolved[0].rule, questions[1].name);
        assert_eq!(
            report.files[0].unresolved[0].reason,
            UnresolvedReason::InsufficientContext
        );
        assert_eq!(report.files[0].input_tokens, Some(123));
        assert_eq!(
            fake.reviews.with(|reviews| reviews.clone()),
            [(
                "lib.rs".to_owned(),
                "fn example() {}\n".to_owned(),
                Some("library".to_owned())
            )]
        );
    }

    #[test]
    fn records_the_hash_of_the_source_it_sent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider::answering(&[Verdict::NoViolation]);
        let report = check(dir.path(), &rules.questions[..1], &fake);
        assert_eq!(
            report.files[0].source_sha256.as_deref(),
            Some(sha256_hex(b"fn example() {}\n").as_str())
        );
        assert!(!report.files[0].cached);
        assert_eq!(report.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn the_review_key_covers_rule_definitions_and_context() {
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..2];
        let key = |questions: &[Question], context| {
            ReviewKey::new(ProviderKind::Decisions, "test-model", questions, context).unwrap()
        };
        let base = key(questions, Some("library"));
        assert_eq!(base, key(questions, Some("library")));
        assert_eq!(
            base.context_sha256.as_deref(),
            Some(sha256_hex(b"library").as_str())
        );
        assert_eq!(key(questions, None).context_sha256, None);
        assert_ne!(base, key(questions, Some("application")));
        assert_ne!(base, key(&questions[..1], Some("library")));

        let mut reworded = questions.to_vec();
        reworded[1].instructions.push_str(" Also consider tests.");
        assert_eq!(reworded[1].name, questions[1].name);
        assert_ne!(base, key(&reworded, Some("library")));
        let mut redescribed = questions.to_vec();
        redescribed[0].choices[0].description.push('.');
        assert_ne!(base, key(&redescribed, Some("library")));

        let other_model =
            ReviewKey::new(ProviderKind::Decisions, "other", questions, Some("library")).unwrap();
        assert_ne!(base, other_model);
    }

    #[test]
    fn a_written_report_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider::answering(&[Verdict::Violation, Verdict::InsufficientContext]);
        let report = check(dir.path(), &rules.questions[..2], &fake);
        let written = serde_json::to_value(&report).unwrap();
        assert_eq!(written["provider"], "decisions");
        assert_eq!(written["files"][0]["status"], "incomplete");
        assert_eq!(
            written["files"][0]["unresolved"][0]["reason"],
            "insufficient_context"
        );
        let read: Report = serde_json::from_value(written.clone()).unwrap();
        assert_eq!(serde_json::to_value(&read).unwrap(), written);
    }

    #[test]
    fn estimates_are_written_only_when_the_provider_gives_them() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let verdicts = [Verdict::Violation, Verdict::InsufficientContext];

        let estimated = check(
            dir.path(),
            &rules.questions[..2],
            &FakeProvider::answering(&verdicts),
        );
        let written = serde_json::to_value(&estimated).unwrap();
        for entry in [
            &written["files"][0]["findings"][0],
            &written["files"][0]["unresolved"][0],
        ] {
            assert_eq!(entry["confidence"], 0.8);
            assert_eq!(entry["probabilities"]["no_violation"], 0.2);
        }

        let unestimated = check(
            dir.path(),
            &rules.questions[..2],
            &FakeProvider {
                estimate: None,
                ..FakeProvider::answering(&verdicts)
            },
        );
        let written = serde_json::to_value(&unestimated).unwrap();
        let finding = written["files"][0]["findings"][0].as_object().unwrap();
        assert_eq!(
            finding.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["guidance", "rule"])
        );
        let unresolved = written["files"][0]["unresolved"][0].as_object().unwrap();
        assert_eq!(
            unresolved
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["reason", "rule"])
        );
        let read: Report = serde_json::from_value(written.clone()).unwrap();
        assert_eq!(read.files[0].findings[0].estimate, None);
        assert_eq!(serde_json::to_value(&read).unwrap(), written);
    }

    #[test]
    fn reads_refusals_written_with_null_estimates() {
        let unresolved: Unresolved = serde_json::from_value(serde_json::json!({
            "rule": "glob_import_context",
            "reason": "refusal",
            "confidence": null,
            "probabilities": null
        }))
        .unwrap();
        assert_eq!(unresolved.estimate, None);
        assert_eq!(
            serde_json::to_value(&unresolved).unwrap(),
            serde_json::json!({ "rule": "glob_import_context", "reason": "refusal" })
        );
    }

    #[test]
    fn refusals_are_unresolved() {
        let rules = RuleSet::load().unwrap();
        let mut file = FileReport::new("lib.rs".into());
        file.record(
            &rules.questions[..1],
            Evaluation {
                answers: vec![Answer::Refusal],
                input_tokens: None,
            },
        )
        .unwrap();
        assert_eq!(file.status, FileStatus::Incomplete);
        assert_eq!(file.unresolved[0].reason, UnresolvedReason::Refusal);
        assert_eq!(file.unresolved[0].estimate, None);
    }

    #[test]
    fn stops_after_a_service_failure_and_marks_remaining_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider::unavailable();
        let report = check(dir.path(), &rules.questions[..1], &fake);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.files[0].status, FileStatus::Error);
        assert_eq!(
            report.files[0].error.as_deref(),
            Some("service unavailable")
        );
        assert_eq!(report.files[1].status, FileStatus::NotChecked);
        assert_eq!(fake.review_count(), 1);
    }

    #[test]
    fn a_wrong_number_of_answers_stops_the_review() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider::answering(&[Verdict::NoViolation]);
        let report = check(dir.path(), &rules.questions[..2], &fake);
        assert_eq!(report.files[0].status, FileStatus::Error);
        assert_eq!(
            report.files[0].error.as_deref(),
            Some("provider returned 1 answers for 2 questions")
        );
        assert!(report.files[0].findings.is_empty());
        assert_eq!(report.files[1].status, FileStatus::NotChecked);
        assert_eq!(fake.review_count(), 1);
    }

    #[test]
    fn input_errors_fail_only_their_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider {
            refuse_input: true,
            ..FakeProvider::answering(&[Verdict::NoViolation])
        };
        let report = check(dir.path(), &rules.questions[..1], &fake);
        assert_eq!(report.status, ReportStatus::Incomplete);
        for file in &report.files {
            assert_eq!(file.status, FileStatus::Error);
            assert_eq!(file.error.as_deref(), Some("too large for this model"));
        }
        assert_eq!(fake.review_count(), 2);
    }

    #[test]
    fn complete_findings_and_clean_results_have_distinct_exit_codes() {
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..1];
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let run = |verdict| check(dir.path(), questions, &FakeProvider::answering(&[verdict]));
        assert_eq!(run(Verdict::Violation).exit_code(), 1);
        let clean = run(Verdict::NoViolation);
        assert_eq!(clean.status, ReportStatus::Complete);
        assert_eq!(clean.exit_code(), 0);
        assert_eq!(clean.files[0].no_violation_count, 1);
    }

    #[test]
    fn files_over_the_provider_limit_are_reported_without_contacting_it() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("large.rs"), vec![b'x'; 65]).unwrap();
        fs::write(dir.path().join("small.rs"), vec![b'x'; 64]).unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeProvider::answering(&[Verdict::NoViolation]);
        let report = check(dir.path(), &rules.questions[..1], &fake);
        assert_eq!(report.status, ReportStatus::Incomplete);
        assert_eq!(report.files[0].status, FileStatus::Error);
        assert!(
            report.files[0]
                .error
                .as_deref()
                .unwrap()
                .contains("input limit")
        );
        assert_eq!(report.files[1].status, FileStatus::Checked);
        assert_eq!(fake.review_count(), 1);
    }
}
