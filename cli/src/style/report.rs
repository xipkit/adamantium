use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::style::{
    provider::{Answer, Evaluation, FileReview, Provider, ProviderKind, ReviewError, Verdict},
    rules::Question,
    scan::Scan,
};

#[derive(Serialize)]
pub(crate) struct Report {
    pub schema_version: u8,
    pub status: &'static str,
    pub provider: ProviderKind,
    pub model: String,
    pub root: String,
    pub selected_rules: Vec<String>,
    pub files: Vec<FileReport>,
}

#[derive(Serialize)]
pub(crate) struct FileReport {
    pub path: String,
    pub scope: &'static str,
    pub status: &'static str,
    pub findings: Vec<Finding>,
    pub unresolved: Vec<Unresolved>,
    pub no_violation_count: usize,
    pub input_tokens: Option<u64>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Finding {
    pub rule: String,
    pub guidance: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

#[derive(Serialize)]
pub(crate) struct Unresolved {
    pub rule: String,
    pub reason: &'static str,
    pub confidence: Option<f64>,
    pub probabilities: Option<BTreeMap<String, f64>>,
}

impl Report {
    pub fn planned(
        scan: &Scan,
        provider: ProviderKind,
        model: &str,
        questions: &[Question],
    ) -> Self {
        Self {
            schema_version: 1,
            status: "planned",
            provider,
            model: model.to_owned(),
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

    pub fn check(
        scan: &Scan,
        kind: ProviderKind,
        model: &str,
        questions: &[Question],
        context: Option<&str>,
        provider: &dyn Provider,
    ) -> Self {
        let mut report = Self::planned(scan, kind, model, questions);
        let mut service_failed = false;
        for (file, path) in report.files.iter_mut().zip(&scan.files) {
            if service_failed {
                file.status = "not_checked";
                continue;
            }
            let source = match read_source(path, provider.max_source_bytes()) {
                Ok(source) => source,
                Err(error) => {
                    file.fail(format!("cannot read source: {error}"));
                    continue;
                }
            };
            let review = FileReview {
                model,
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
        report.status = if report.files.iter().all(|file| file.status == "checked") {
            "complete"
        } else {
            "incomplete"
        };
        report
    }

    pub fn exit_code(&self) -> u8 {
        if self.status == "incomplete" {
            2
        } else if self.files.iter().any(|file| !file.findings.is_empty()) {
            1
        } else {
            0
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
            scope: "file",
            status: "planned",
            findings: Vec::new(),
            unresolved: Vec::new(),
            no_violation_count: 0,
            input_tokens: None,
            error: None,
        }
    }

    fn fail(&mut self, error: String) {
        self.status = "error";
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
                    reason: "refusal",
                    confidence: None,
                    probabilities: None,
                }),
                Answer::Choice {
                    verdict: Verdict::Violation,
                    confidence,
                    probabilities,
                } => self.findings.push(Finding {
                    rule,
                    guidance: question.instructions.clone(),
                    confidence,
                    probabilities,
                }),
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
                    reason: "insufficient_context",
                    confidence: Some(confidence),
                    probabilities: Some(probabilities),
                }),
            }
        }
        self.status = if self.unresolved.is_empty() {
            "checked"
        } else {
            "incomplete"
        };
        self.input_tokens = evaluation.input_tokens;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use adamantium::Actor;
    use anyhow::anyhow;

    use super::*;
    use crate::style::rules::RuleSet;

    /// Answers every file with the same verdicts, or fails as configured.
    struct FakeProvider {
        answers: Option<Vec<Verdict>>,
        refuse_input: bool,
        reviews: Actor<Vec<(String, String, Option<String>)>>,
    }

    impl FakeProvider {
        fn answering(verdicts: &[Verdict]) -> Self {
            Self {
                answers: Some(verdicts.to_vec()),
                refuse_input: false,
                reviews: Actor::default(),
            }
        }

        fn unavailable() -> Self {
            Self {
                answers: None,
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
                        confidence: 0.8,
                        probabilities: BTreeMap::from([
                            ("violation".to_owned(), 0.7),
                            ("no_violation".to_owned(), 0.2),
                            ("insufficient_context".to_owned(), 0.1),
                        ]),
                    })
                    .collect(),
                input_tokens: Some(123),
            })
        }
    }

    fn check(dir: &Path, questions: &[Question], provider: &FakeProvider) -> Report {
        let scan = Scan::discover(dir).unwrap();
        Report::check(
            &scan,
            ProviderKind::Decisions,
            "test-model",
            questions,
            Some("library"),
            provider,
        )
    }

    #[test]
    fn reports_file_scoped_findings_and_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..2];
        let fake = FakeProvider::answering(&[Verdict::Violation, Verdict::InsufficientContext]);
        let report = check(dir.path(), questions, &fake);
        assert_eq!(report.status, "incomplete");
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.provider, ProviderKind::Decisions);
        assert_eq!(report.model, "test-model");
        assert_eq!(report.files[0].scope, "file");
        assert_eq!(report.files[0].path, "lib.rs");
        assert_eq!(report.files[0].findings[0].rule, questions[0].name);
        assert_eq!(
            report.files[0].findings[0].guidance,
            questions[0].instructions
        );
        assert_eq!(report.files[0].unresolved[0].rule, questions[1].name);
        assert_eq!(report.files[0].unresolved[0].reason, "insufficient_context");
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
        assert_eq!(file.status, "incomplete");
        assert_eq!(file.unresolved[0].reason, "refusal");
        assert_eq!(file.unresolved[0].confidence, None);
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
        assert_eq!(report.files[0].status, "error");
        assert_eq!(
            report.files[0].error.as_deref(),
            Some("service unavailable")
        );
        assert_eq!(report.files[1].status, "not_checked");
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
        assert_eq!(report.files[0].status, "error");
        assert_eq!(
            report.files[0].error.as_deref(),
            Some("provider returned 1 answers for 2 questions")
        );
        assert!(report.files[0].findings.is_empty());
        assert_eq!(report.files[1].status, "not_checked");
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
        assert_eq!(report.status, "incomplete");
        for file in &report.files {
            assert_eq!(file.status, "error");
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
        assert_eq!(clean.status, "complete");
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
        assert_eq!(report.status, "incomplete");
        assert_eq!(report.files[0].status, "error");
        assert!(
            report.files[0]
                .error
                .as_deref()
                .unwrap()
                .contains("input limit")
        );
        assert_eq!(report.files[1].status, "checked");
        assert_eq!(fake.review_count(), 1);
    }
}
