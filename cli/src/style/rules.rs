use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// A set of rules evaluated together, embedded in the binary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Pack {
    /// The judgment-based parts of the Google Rust style guide.
    #[default]
    Google,
    /// Concurrency bugs that Clippy and the Dylint lints can't see. For now
    /// only state with a joint invariant split across separate Actors, locks
    /// or atomics: the other evaluated rules flagged correct code when asked
    /// about whole files.
    Concurrency,
}

impl Pack {
    fn definitions(self) -> &'static str {
        match self {
            Self::Google => include_str!("google-rust-decisions.json"),
            Self::Concurrency => include_str!("concurrency-decisions.json"),
        }
    }
}

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

impl Question {
    /// The description of the `violation` choice: what is wrong with code
    /// that breaks the rule.
    pub fn violation(&self) -> Option<&str> {
        self.choices
            .iter()
            .find(|choice| choice.value == "violation")
            .map(|choice| choice.description.as_str())
    }
}

impl RuleSet {
    /// Reads and validates the rules of `pack`.
    pub fn load(pack: Pack) -> Result<Self> {
        let rules: Self =
            serde_json::from_str(pack.definitions()).context("invalid embedded rule set")?;
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
        let rules = RuleSet::load(Pack::Google).unwrap();
        assert_eq!(rules.model, "gpt-6-luna");
        assert_eq!(rules.questions.len(), 51);
        let selected = rules
            .select(&["glob_import_context".into(), "glob_import_context".into()])
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "glob_import_context");
        assert!(rules.select(&["not_a_rule".into()]).is_err());
    }

    #[test]
    fn the_concurrency_pack_ships_only_the_rule_that_held_up_on_whole_files() {
        let rules = RuleSet::load(Pack::Concurrency).unwrap();
        assert_eq!(rules.model, "gpt-6-luna");
        let names: Vec<_> = rules.questions.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["split_actor_invariant"]);
    }

    /// The evaluation's precision and recall hold only for the wording it
    /// sent, so each shipped rule must match its evaluated definition byte
    /// for byte.
    #[test]
    fn shipped_concurrency_rules_match_the_evaluated_definitions() {
        let evaluated_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../style/rules/concurrency-decisions.json");
        let evaluated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(evaluated_path).unwrap()).unwrap();
        let shipped: serde_json::Value =
            serde_json::from_str(Pack::Concurrency.definitions()).unwrap();
        assert_eq!(shipped["model"], evaluated["model"]);
        let shipped = shipped["questions"].as_array().unwrap();
        assert_eq!(shipped.len(), 1);
        for question in shipped {
            let name = &question["name"];
            let original = evaluated["questions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["name"] == *name)
                .unwrap();
            assert_eq!(
                serde_json::to_string(question).unwrap(),
                serde_json::to_string(original).unwrap(),
                "{name} differs from its evaluated wording"
            );
        }
    }
}
