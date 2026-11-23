use std::{collections::BTreeMap, fmt, fs::File, io::Read, path::Path};

use crate::sha256_hex;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::style::{
    provider::{
        Answer, Evaluation, FileReview, LineRange, Locations, Provider, ProviderKind, ReviewError,
        Verdict,
    },
    rules::{Pack, Question},
    scan::Scan,
};

pub(crate) const SCHEMA_VERSION: u8 = 4;

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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewKey {
    pub provider: ProviderKind,
    pub pack: Pack,
    pub model: String,
    pub rules_sha256: String,
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
    pub status: FileStatus,
    pub cached: bool,
    pub source_sha256: Option<String>,
    pub findings: Vec<Finding>,
    pub unresolved: Vec<Unresolved>,
    pub no_violation_count: usize,
    pub input_tokens: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileStatus {
    Planned,
    Checked,
    Incomplete,
    Error,
    NotChecked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Finding {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    pub guidance: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Unresolved {
    pub rule: String,
    pub reason: UnresolvedReason,
    pub confidence: Option<f64>,
    pub probabilities: Option<BTreeMap<String, f64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnresolvedReason {
    Refusal,
    InsufficientContext,
}

impl ReviewKey {
    /// Identifies a review by provider, pack, model, rules, and context.
    pub fn new(
        provider: ProviderKind,
        pack: Pack,
        model: &str,
        questions: &[Question],
        context: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            provider,
            pack,
            model: model.to_owned(),
            rules_sha256: sha256_hex(&serde_json::to_vec(questions)?),
            context_sha256: context.map(|context| sha256_hex(context.as_bytes())),
        })
    }
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

    /// Sends each planned file to the provider, in scan order, asks it to
    /// locate any findings, then settles the report's status. `scan` must be
    /// the scan the report was planned from.
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
            match file.review(provider, &self.key.model, questions, &source, context) {
                Ok(()) => {}
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
        self.findings.clear();
        self.unresolved.clear();
        self.no_violation_count = 0;
        self.error = Some(error);
    }

    fn review(
        &mut self,
        provider: &dyn Provider,
        model: &str,
        questions: &[Question],
        source: &str,
        context: Option<&str>,
    ) -> Result<(), ReviewError> {
        let path = self.path.clone();
        let review = FileReview {
            model,
            questions,
            path: &path,
            source,
            context,
        };
        let evaluation = provider.review(&review)?;
        self.record(questions, evaluation)
            .map_err(ReviewError::Service)?;
        let violated = self.violated(questions);
        if violated.is_empty() {
            return Ok(());
        }
        let locations = provider
            .locate(&FileReview {
                questions: &violated,
                ..review
            })
            .inspect_err(|_| {
                self.input_tokens = None;
            })?;
        self.place(locations).map_err(ReviewError::Service)
    }

    fn violated(&self, questions: &[Question]) -> Vec<Question> {
        self.findings
            .iter()
            .filter_map(|finding| questions.iter().find(|q| q.name == finding.rule))
            .cloned()
            .collect()
    }

    fn place(&mut self, locations: Locations) -> Result<()> {
        self.input_tokens = self
            .input_tokens
            .zip(locations.input_tokens)
            .map(|(review, locate)| review + locate);
        ensure!(
            locations.lines.len() == self.findings.len(),
            "provider located {} findings of {}",
            locations.lines.len(),
            self.findings.len()
        );
        for (finding, lines) in self.findings.iter_mut().zip(locations.lines) {
            finding.lines = lines;
        }
        Ok(())
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
                    confidence: None,
                    probabilities: None,
                }),
                Answer::Choice {
                    verdict: Verdict::Violation,
                    confidence,
                    probabilities,
                } => {
                    let guidance = question
                        .violation()
                        .with_context(|| format!("rule {rule} has no violation choice"))?
                        .to_owned();
                    self.findings.push(Finding {
                        rule,
                        lines: None,
                        guidance,
                        confidence,
                        probabilities,
                    });
                }
                Answer::Choice {
                    verdict: Verdict::NoViolation,
                    ..
                } => self.no_violation_count += 1,
                Answer::Choice {
                    verdict: Verdict::InsufficientContext,
                    confidence,
                    probabilities,
                } => self.unresolved.push(Unresolved {
                    rule,
                    reason: UnresolvedReason::InsufficientContext,
                    confidence: Some(confidence),
                    probabilities: Some(probabilities),
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

impl FileStatus {
    /// Reads a status from its name in a report.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "planned" => Self::Planned,
            "checked" => Self::Checked,
            "incomplete" => Self::Incomplete,
            "error" => Self::Error,
            "not_checked" => Self::NotChecked,
            _ => return None,
        })
    }
}

impl ReportStatus {
    /// Reads a status from its name in a report.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "planned" => Self::Planned,
            "incomplete" => Self::Incomplete,
            "complete" => Self::Complete,
            _ => return None,
        })
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
    use std::fs;

    use super::*;
    use crate::style::{
        fake::{FakeLocations, FakeProvider},
        rules::RuleSet,
    };

    fn check(dir: &Path, questions: &[Question], provider: &FakeProvider) -> Report {
        let scan = Scan::discover(dir).unwrap();
        let key = ReviewKey::new(
            ProviderKind::Decisions,
            Pack::Google,
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
        let rules = RuleSet::load(Pack::Google).unwrap();
        let questions = &rules.questions[..2];
        let fake = FakeProvider::answering(&[Verdict::Violation, Verdict::InsufficientContext]);
        let report = check(dir.path(), questions, &fake);
        assert_eq!(report.status, ReportStatus::Incomplete);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.key.provider, ProviderKind::Decisions);
        assert_eq!(report.key.model, "test-model");
        assert_eq!(report.files[0].path, "lib.rs");
        assert_eq!(report.files[0].findings[0].rule, questions[0].name);
        assert_eq!(
            report.files[0].findings[0].guidance,
            questions[0].violation().unwrap()
        );
        assert_ne!(
            report.files[0].findings[0].guidance,
            questions[0].instructions
        );
        assert_eq!(report.files[0].unresolved[0].rule, questions[1].name);
        assert_eq!(
            report.files[0].unresolved[0].reason,
            UnresolvedReason::InsufficientContext
        );
        assert_eq!(report.files[0].findings[0].lines, None);
        assert_eq!(report.files[0].input_tokens, Some(130));
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
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        let rules = RuleSet::load(Pack::Google).unwrap();
        let questions = &rules.questions[..2];
        let key = |questions: &[Question], context| {
            ReviewKey::new(
                ProviderKind::Decisions,
                Pack::Google,
                "test-model",
                questions,
                context,
            )
            .unwrap()
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

        let other_model = ReviewKey::new(
            ProviderKind::Decisions,
            Pack::Google,
            "other",
            questions,
            Some("library"),
        )
        .unwrap();
        assert_ne!(base, other_model);
    }

    #[test]
    fn a_written_report_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
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
    fn refusals_are_unresolved() {
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        assert_eq!(file.unresolved[0].confidence, None);
    }

    #[test]
    fn a_violated_rule_without_a_violation_choice_is_an_error() {
        let rules = RuleSet::load(Pack::Google).unwrap();
        let mut question = rules.questions[0].clone();
        question
            .choices
            .retain(|choice| choice.value != "violation");
        let mut file = FileReport::new("lib.rs".into());
        let error = file
            .record(
                &[question],
                Evaluation {
                    answers: vec![Answer::Choice {
                        verdict: Verdict::Violation,
                        confidence: 0.9,
                        probabilities: BTreeMap::new(),
                    }],
                    input_tokens: None,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("has no violation choice"));
    }

    #[test]
    fn stops_after_a_service_failure_and_marks_remaining_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        let rules = RuleSet::load(Pack::Google).unwrap();
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
        let rules = RuleSet::load(Pack::Google).unwrap();
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

    #[test]
    fn locates_only_the_rules_a_file_breaks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
        let questions = &rules.questions[..3];
        let lines = LineRange {
            start_line: 4,
            end_line: 6,
        };
        let fake = FakeProvider {
            locations: FakeLocations::Fixed(vec![Some(lines), None]),
            ..FakeProvider::answering(&[
                Verdict::Violation,
                Verdict::NoViolation,
                Verdict::Violation,
            ])
        };
        let report = check(dir.path(), questions, &fake);
        assert_eq!(
            fake.locates.with(|locates| locates.clone()),
            [vec![questions[0].name.clone(), questions[2].name.clone()]]
        );
        let file = &report.files[0];
        assert_eq!(file.status, FileStatus::Checked);
        assert_eq!(file.findings[0].lines, Some(lines));
        assert_eq!(file.findings[1].lines, None);
        assert_eq!(file.input_tokens, Some(130));

        let written = serde_json::to_value(&report).unwrap();
        let findings = &written["files"][0]["findings"];
        assert_eq!(
            findings[0]["lines"],
            serde_json::json!({ "start_line": 4, "end_line": 6 })
        );
        assert!(findings[1].get("lines").is_none());
        let read: Report = serde_json::from_value(written).unwrap();
        assert_eq!(read.files[0].findings[0].lines, Some(lines));
        assert_eq!(read.files[0].findings[1].lines, None);
    }

    #[test]
    fn a_file_without_findings_is_not_located() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
        let fake = FakeProvider::answering(&[Verdict::NoViolation, Verdict::InsufficientContext]);
        let report = check(dir.path(), &rules.questions[..2], &fake);
        assert_eq!(fake.locates.with(|locates| locates.len()), 0);
        assert_eq!(report.files[0].input_tokens, Some(123));
    }

    #[test]
    fn a_failure_to_locate_fails_the_file_and_stops_the_review() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
        let fake = FakeProvider {
            locations: FakeLocations::Unavailable,
            ..FakeProvider::answering(&[Verdict::Violation])
        };
        let report = check(dir.path(), &rules.questions[..1], &fake);
        let file = &report.files[0];
        assert_eq!(file.status, FileStatus::Error);
        assert_eq!(file.error.as_deref(), Some("service unavailable"));
        assert!(file.findings.is_empty());
        assert_eq!(file.input_tokens, None);
        assert_eq!(report.files[1].status, FileStatus::NotChecked);
        assert_eq!(fake.review_count(), 1);
    }

    #[test]
    fn the_wrong_number_of_locations_fails_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load(Pack::Google).unwrap();
        let fake = FakeProvider {
            locations: FakeLocations::Fixed(Vec::new()),
            ..FakeProvider::answering(&[Verdict::Violation])
        };
        let report = check(dir.path(), &rules.questions[..1], &fake);
        assert_eq!(report.files[0].status, FileStatus::Error);
        assert_eq!(
            report.files[0].error.as_deref(),
            Some("provider located 0 findings of 1")
        );
        assert!(report.files[0].findings.is_empty());
        assert_eq!(report.files[0].input_tokens, Some(130));
    }
}
