use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct RuleSet {
    pub model: String,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Question {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub instructions: String,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Choice {
    pub value: String,
    pub description: String,
}

impl RuleSet {
    pub fn load() -> Result<Self> {
        let rules: Self = serde_json::from_str(include_str!(
            "../../../style/rules/google-rust-decisions.json"
        ))
        .context("invalid embedded rule set")?;
        if rules.model.is_empty() || rules.questions.is_empty() || rules.questions.len() > 400 {
            bail!("the rule set must contain a model and between 1 and 400 questions");
        }

        let mut names = HashSet::new();
        for question in &rules.questions {
            let values: HashSet<&str> = question.choices.iter().map(|c| c.value.as_str()).collect();
            if question.kind != "choice"
                || question.name.is_empty()
                || question.instructions.is_empty()
                || !names.insert(question.name.as_str())
                || values != HashSet::from(["violation", "no_violation", "insufficient_context"])
                || question.choices.len() != values.len()
            {
                bail!("invalid or duplicate question: {}", question.name);
            }
        }
        Ok(rules)
    }

    pub fn select(&self, names: &[String]) -> Result<Vec<Question>> {
        if names.is_empty() {
            return Ok(self.questions.clone());
        }
        let requested: HashSet<&str> = names.iter().map(String::as_str).collect();
        for name in &requested {
            if !self.questions.iter().any(|question| question.name == *name) {
                bail!("unknown rule: {name}");
            }
        }
        Ok(self
            .questions
            .iter()
            .filter(|question| requested.contains(question.name.as_str()))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_rules_are_valid_and_can_be_selected() {
        let rules = RuleSet::load().unwrap();
        assert_eq!(rules.model, "gpt-6-luna");
        assert_eq!(rules.questions.len(), 51);
        let selected = rules
            .select(&["glob_import_context".into(), "glob_import_context".into()])
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "glob_import_context");
        assert!(rules.select(&["not_a_rule".into()]).is_err());
    }
}
