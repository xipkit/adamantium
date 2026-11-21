use std::collections::BTreeMap;

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::style::{decisions::Decisions, rules::Question};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[value(
        help = "The OpenAI Decisions API. Needs OPENAI_API_KEY or, on macOS, an `openai-api-key` Keychain item"
    )]
    #[default]
    Decisions,
}

impl ProviderKind {
    pub(crate) fn connect(self) -> Result<Box<dyn Provider>> {
        match self {
            Self::Decisions => Ok(Box::new(
                Decisions::new(crate::style::decisions::api_key()?),
            )),
        }
    }
}

pub(crate) trait Provider {
    fn max_source_bytes(&self) -> usize;

    fn review(&self, file: &FileReview<'_>) -> Result<Evaluation, ReviewError>;

    fn locate(&self, file: &FileReview<'_>) -> Result<Locations, ReviewError>;
}

pub(crate) struct FileReview<'a> {
    pub model: &'a str,
    pub questions: &'a [Question],
    pub path: &'a str,
    pub source: &'a str,
    pub context: Option<&'a str>,
}

pub(crate) struct Evaluation {
    pub answers: Vec<Answer>,
    pub input_tokens: Option<u64>,
}

pub(crate) struct Locations {
    pub lines: Vec<Option<LineRange>>,
    pub input_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct LineRange {
    pub start_line: usize,
    pub end_line: usize,
}

pub(crate) enum Answer {
    Choice {
        verdict: Verdict,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Refusal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Violation,
    NoViolation,
    InsufficientContext,
}

impl Verdict {
    /// Parses the value of one of the rule set's three choices.
    pub fn from_choice(value: &str) -> Option<Self> {
        match value {
            "violation" => Some(Self::Violation),
            "no_violation" => Some(Self::NoViolation),
            "insufficient_context" => Some(Self::InsufficientContext),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ReviewError {
    Input(anyhow::Error),
    Service(anyhow::Error),
}
