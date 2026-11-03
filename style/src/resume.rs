use std::{collections::BTreeSet, fs::File, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::{rules::Question, scan::Scan};

#[derive(Deserialize)]
struct PreviousReport {
    schema_version: u8,
    status: String,
    model: String,
    root: String,
    selected_rules: Vec<String>,
    files: Vec<PreviousFile>,
}

#[derive(Deserialize)]
struct PreviousFile {
    path: String,
    status: String,
}

pub(crate) fn select_pending(
    scan: &mut Scan,
    previous_path: &Path,
    model: &str,
    questions: &[Question],
) -> Result<()> {
    let previous_file = File::open(previous_path)
        .with_context(|| format!("cannot open previous report {}", previous_path.display()))?;
    let previous: PreviousReport =
        serde_json::from_reader(previous_file).context("invalid previous JSON report")?;
    ensure!(
        previous.schema_version == 1,
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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};

    use super::*;
    use crate::rules::RuleSet;

    fn previous_report(root: &Path, model: &str, question: &Question) -> Value {
        json!({
            "schema_version": 1,
            "status": "incomplete",
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
        select_pending(&mut scan, &report_path, &rules.model, questions).unwrap();
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
            select_pending(&mut scan, &report_path, &rules.model, questions)
        };

        let mut wrong = valid.clone();
        wrong["root"] = json!("/different/root");
        assert!(check(&wrong).unwrap_err().to_string().contains("scan root"));
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
}
