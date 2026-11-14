use std::collections::BTreeMap;

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::style::{decisions::Decisions, rules::Question};

/// The service that answers the style questions for a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The OpenAI Decisions API. Needs OPENAI_API_KEY or, on macOS, an
    /// `openai-api-key` Keychain item.
    #[default]
    Decisions,
}

impl ProviderKind {
    /// Builds a client for this provider, reading its credentials.
    pub(crate) fn connect(self) -> Result<Box<dyn Provider>> {
        match self {
            Self::Decisions => Ok(Box::new(
                Decisions::new(crate::style::decisions::api_key()?),
            )),
        }
    }
}

/// A source of answers to the style questions.
pub(crate) trait Provider {
    /// The largest source file, in bytes, the provider can accept.
    fn max_source_bytes(&self) -> usize;

    /// Answers every question for one file, in question order.
    fn review(&self, file: &FileReview<'_>) -> Result<Evaluation, ReviewError>;

    /// Finds the lines that violate each question's rule in a file the
    /// provider judged to violate all of them, in question order.
    fn locate(&self, file: &FileReview<'_>) -> Result<Locations, ReviewError>;
}

/// One file and the questions to ask about it.
pub(crate) struct FileReview<'a> {
    pub model: &'a str,
    pub questions: &'a [Question],
    pub path: &'a str,
    pub source: &'a str,
    pub context: Option<&'a str>,
}

/// A provider's answers for one file.
pub(crate) struct Evaluation {
    /// One answer per question, in the order the questions were asked.
    pub answers: Vec<Answer>,
    pub input_tokens: Option<u64>,
}

/// Where in a file each violated rule is broken.
pub(crate) struct Locations {
    /// One entry per question, in question order: `None` when the provider
    /// gave no confident range, including when the violation concerns the
    /// file as a whole.
    pub lines: Vec<Option<LineRange>>,
    pub input_tokens: Option<u64>,
}

/// Lines `start_line` through `end_line` of a file, numbered from 1.
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

/// Why a provider gave no answers for a file.
#[derive(Debug)]
pub(crate) enum ReviewError {
    /// This file can't be sent, but other files can.
    Input(anyhow::Error),
    /// The service failed, so no further files should be sent.
    Service(anyhow::Error),
}
