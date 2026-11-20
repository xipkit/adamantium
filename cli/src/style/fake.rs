use std::collections::BTreeMap;

use adamantium::Actor;
use anyhow::{Result, anyhow};

use crate::style::provider::{
    Answer, Evaluation, FileReview, LineRange, Locations, Provider, ReviewError, Verdict,
};

pub(crate) struct FakeProvider {
    pub answers: Option<Vec<Verdict>>,
    pub refuse_input: bool,
    pub locations: FakeLocations,
    pub reviews: Actor<Vec<(String, String, Option<String>)>>,
    pub locates: Actor<Vec<Vec<String>>>,
}

pub(crate) enum FakeLocations {
    Unplaced,
    Fixed(Vec<Option<LineRange>>),
    Unavailable,
}

impl FakeProvider {
    /// Returns the given verdicts for each review.
    pub fn answering(verdicts: &[Verdict]) -> Self {
        Self {
            answers: Some(verdicts.to_vec()),
            refuse_input: false,
            locations: FakeLocations::Unplaced,
            reviews: Actor::default(),
            locates: Actor::default(),
        }
    }

    /// Returns a service error on review.
    pub fn unavailable() -> Self {
        Self {
            answers: None,
            refuse_input: false,
            locations: FakeLocations::Unplaced,
            reviews: Actor::default(),
            locates: Actor::default(),
        }
    }

    /// Counts attempted reviews.
    pub fn review_count(&self) -> usize {
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

    fn locate(&self, file: &FileReview<'_>) -> Result<Locations, ReviewError> {
        self.locates.with(|locates| {
            locates.push(file.questions.iter().map(|q| q.name.clone()).collect());
        });
        let lines = match &self.locations {
            FakeLocations::Unplaced => vec![None; file.questions.len()],
            FakeLocations::Fixed(lines) => lines.clone(),
            FakeLocations::Unavailable => {
                return Err(ReviewError::Service(anyhow!("service unavailable")));
            }
        };
        Ok(Locations {
            lines,
            input_tokens: Some(7),
        })
    }
}
