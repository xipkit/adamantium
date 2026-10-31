use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::{
    api::{self, Decisions},
    rules::Question,
    scan::Scan,
};

#[derive(Serialize)]
pub(crate) struct Report {
    pub schema_version: u8,
    pub status: &'static str,
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
    pub fn planned(scan: &Scan, model: &str, questions: &[Question]) -> Self {
        Self {
            schema_version: 1,
            status: "planned",
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
        model: &str,
        questions: &[Question],
        context: Option<&str>,
        client: &impl Decisions,
    ) -> Self {
        let mut report = Self::planned(scan, model, questions);
        let mut service_failed = false;
        for (file, path) in report.files.iter_mut().zip(&scan.files) {
            if service_failed {
                file.status = "not_checked";
                continue;
            }
            let source = match read_source(path) {
                Ok(source) => source,
                Err(error) => {
                    file.fail(format!("cannot read source: {error}"));
                    continue;
                }
            };
            let request = match api::request(model, questions, &file.path, &source, context) {
                Ok(request) => request,
                Err(error) => {
                    file.fail(error.to_string());
                    continue;
                }
            };
            let response = match client.evaluate(&request) {
                Ok(response) => response,
                Err(error) => {
                    file.fail(error.to_string());
                    service_failed = true;
                    continue;
                }
            };
            match interpret(&response, questions) {
                Ok(outcomes) => {
                    file.status = if outcomes.unresolved.is_empty() {
                        "checked"
                    } else {
                        "incomplete"
                    };
                    file.findings = outcomes.findings;
                    file.unresolved = outcomes.unresolved;
                    file.no_violation_count = outcomes.no_violation_count;
                    file.input_tokens = response["usage"]["input_tokens"].as_u64();
                }
                Err(error) => {
                    file.fail(format!("invalid Decisions response: {error}"));
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

fn read_source(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((api::MAX_JSON_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= api::MAX_JSON_INPUT_BYTES,
        "file exceeds the Decisions API input limit"
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
}

struct Outcomes {
    findings: Vec<Finding>,
    unresolved: Vec<Unresolved>,
    no_violation_count: usize,
}

fn interpret(response: &Value, questions: &[Question]) -> Result<Outcomes> {
    let answers = response["answers"]
        .as_array()
        .context("answers must be an array")?;
    ensure!(answers.len() == questions.len(), "wrong number of answers");
    let mut by_name = BTreeMap::new();
    for answer in answers {
        let name = answer["name"]
            .as_str()
            .context("answer is missing its name")?;
        ensure!(
            by_name.insert(name, answer).is_none(),
            "duplicate answer: {name}"
        );
    }
    let mut outcomes = Outcomes {
        findings: Vec::new(),
        unresolved: Vec::new(),
        no_violation_count: 0,
    };
    for question in questions {
        let answer = by_name
            .remove(question.name.as_str())
            .context("missing named answer")?;
        if answer["type"] == "refusal" {
            outcomes.unresolved.push(Unresolved {
                rule: question.name.clone(),
                reason: "refusal",
                confidence: None,
                probabilities: None,
            });
            continue;
        }
        ensure!(answer["type"] == "choice", "unexpected answer type");
        let confidence = probability(&answer["confidence"])?;
        let entries = answer["probabilities"]
            .as_array()
            .context("missing probabilities")?;
        let mut probabilities = BTreeMap::new();
        for entry in entries {
            let name = entry["value"]
                .as_str()
                .context("invalid probability label")?;
            ensure!(
                question.choices.iter().any(|choice| choice.value == name),
                "unexpected probability label"
            );
            ensure!(
                probabilities
                    .insert(name.to_owned(), probability(&entry["probability"])?)
                    .is_none(),
                "duplicate probability label"
            );
        }
        ensure!(
            probabilities.len() == question.choices.len(),
            "missing choice probability"
        );
        match answer["choice"].as_str() {
            Some("violation") => outcomes.findings.push(Finding {
                rule: question.name.clone(),
                guidance: question.instructions.clone(),
                confidence,
                probabilities,
            }),
            Some("no_violation") => outcomes.no_violation_count += 1,
            Some("insufficient_context") => outcomes.unresolved.push(Unresolved {
                rule: question.name.clone(),
                reason: "insufficient_context",
                confidence: Some(confidence),
                probabilities: Some(probabilities),
            }),
            _ => bail!("invalid selected choice"),
        }
    }
    ensure!(by_name.is_empty(), "unexpected named answer");
    Ok(outcomes)
}

fn probability(value: &Value) -> Result<f64> {
    let value = value.as_f64().context("missing probability")?;
    ensure!((0.0..=1.0).contains(&value), "probability outside [0, 1]");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs};

    use serde_json::json;

    use super::*;
    use crate::rules::RuleSet;

    struct FakeDecisions {
        response: Option<Value>,
        requests: RefCell<Vec<Value>>,
    }

    impl Decisions for FakeDecisions {
        fn evaluate(&self, request: &Value) -> Result<Value> {
            self.requests.borrow_mut().push(request.clone());
            self.response.clone().context("service unavailable")
        }
    }

    fn choice(name: &str, selected: &str) -> Value {
        json!({
            "type": "choice", "name": name, "choice": selected, "confidence": 0.8,
            "probabilities": [
                { "value": "violation", "probability": 0.7 },
                { "value": "no_violation", "probability": 0.2 },
                { "value": "insufficient_context", "probability": 0.1 }
            ]
        })
    }

    #[test]
    fn reports_file_scoped_findings_and_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let scan = Scan::discover(dir.path()).unwrap();
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..2];
        let fake = FakeDecisions {
            response: Some(json!({ "answers": [
                choice(&questions[0].name, "violation"),
                choice(&questions[1].name, "insufficient_context")
            ], "usage": { "input_tokens": 123 } })),
            requests: RefCell::new(Vec::new()),
        };
        let report = Report::check(&scan, &rules.model, questions, Some("library"), &fake);
        assert_eq!(report.status, "incomplete");
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.files[0].scope, "file");
        assert_eq!(report.files[0].path, "lib.rs");
        assert_eq!(report.files[0].findings[0].rule, questions[0].name);
        assert_eq!(report.files[0].unresolved[0].reason, "insufficient_context");
        assert_eq!(report.files[0].input_tokens, Some(123));
        assert_eq!(fake.requests.borrow().len(), 1);
        assert_eq!(
            fake.requests.borrow()[0]["input"][1]["content"][0]["data"]["source"],
            "fn example() {}\n"
        );
        assert_eq!(
            fake.requests.borrow()[0]["input"][1]["content"][0]["data"]["project_context"],
            "library"
        );
    }

    #[test]
    fn refusals_are_unresolved_and_invalid_answers_are_rejected() {
        let rules = RuleSet::load().unwrap();
        let question = &rules.questions[..1];
        let refusal = json!({ "answers": [{ "type": "refusal", "name": question[0].name }] });
        assert_eq!(
            interpret(&refusal, question).unwrap().unresolved[0].reason,
            "refusal"
        );
        let missing = json!({ "answers": [] });
        assert!(interpret(&missing, question).is_err());
        let wrong_name = json!({ "answers": [choice("wrong", "no_violation")] });
        assert!(interpret(&wrong_name, question).is_err());
        let mut bad_probability = choice(&question[0].name, "violation");
        bad_probability["confidence"] = json!(1.5);
        assert!(interpret(&json!({ "answers": [bad_probability] }), question).is_err());
    }

    #[test]
    fn stops_after_a_service_failure_and_marks_remaining_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let scan = Scan::discover(dir.path()).unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeDecisions {
            response: None,
            requests: RefCell::new(Vec::new()),
        };
        let report = Report::check(&scan, &rules.model, &rules.questions[..1], None, &fake);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.files[0].status, "error");
        assert_eq!(report.files[1].status, "not_checked");
        assert_eq!(fake.requests.borrow().len(), 1);
    }

    #[test]
    fn complete_findings_and_clean_results_have_distinct_exit_codes() {
        let rules = RuleSet::load().unwrap();
        let questions = &rules.questions[..1];
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let scan = Scan::discover(dir.path()).unwrap();
        let run = |selected| {
            let fake = FakeDecisions {
                response: Some(json!({ "answers": [choice(&questions[0].name, selected)] })),
                requests: RefCell::new(Vec::new()),
            };
            Report::check(&scan, &rules.model, questions, None, &fake)
        };
        assert_eq!(run("violation").exit_code(), 1);
        let clean = run("no_violation");
        assert_eq!(clean.exit_code(), 0);
        assert_eq!(clean.files[0].no_violation_count, 1);
    }

    #[test]
    fn oversized_files_are_reported_without_contacting_the_service() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("large.rs"),
            vec![b'x'; api::MAX_JSON_INPUT_BYTES + 1],
        )
        .unwrap();
        let scan = Scan::discover(dir.path()).unwrap();
        let rules = RuleSet::load().unwrap();
        let fake = FakeDecisions {
            response: None,
            requests: RefCell::new(Vec::new()),
        };
        let report = Report::check(&scan, &rules.model, &rules.questions[..1], None, &fake);
        assert_eq!(report.status, "incomplete");
        assert_eq!(report.files[0].status, "error");
        assert!(
            report.files[0]
                .error
                .as_deref()
                .unwrap()
                .contains("input limit")
        );
        assert!(fake.requests.borrow().is_empty());
    }
}
