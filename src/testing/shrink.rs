use std::{future::Future, sync::Arc};

use super::{FailureKind, Replay, ReplayPolicy, TimelineEvent, play};
#[cfg(doc)]
use super::{explain, replay};

/// Minimizes a failing schedule by removing chunks of steps and simplifying
/// task switches. It prefers fewer steps, then fewer switches, and stops when
/// no tried chunk, single-step deletion or switch edit improves the schedule.
///
/// Each attempt runs a fresh factory with the original data seed and step
/// limit. Reductions must reproduce the original failure category and message.
/// Panics must also end in the same last scheduled task. Failures with opaque panic
/// payloads are returned unchanged because their identity cannot be compared.
///
/// Returns `None` if the record passes or diverges under the factory. Panics
/// on incompatible metadata, like [`replay`]. The returned record uses the
/// ordinary replay format and can be passed to [`explain`] or [`Replay::save`].
pub fn shrink<F, Fut>(record: &Replay, factory: F) -> Option<Replay>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    record.assert_valid();

    let factory = Arc::new(factory);
    let probe = |candidate: &Replay| {
        let factory = Arc::clone(&factory);
        play(candidate, ReplayPolicy::Shrink, move || factory()).err()
    };
    let baseline = probe(record)?;
    if baseline.replay() != record {
        return None;
    }
    let Some(message) = baseline.shrink_message.as_deref() else {
        return Some(record.clone());
    };

    let mut best = record.clone();
    loop {
        let cost = schedule_cost(&best);
        let improve = |candidate: &Replay| {
            let failure = probe(candidate)?;
            let same_task = baseline.kind != FailureKind::Panic
                || failure.timeline.events.last().map(TimelineEvent::kind)
                    == baseline.timeline.events.last().map(TimelineEvent::kind);
            (failure.kind == baseline.kind
                && failure.shrink_message.as_deref() == Some(message)
                && same_task
                && schedule_cost(failure.replay()) < cost)
                .then_some(failure.timeline.replay)
        };
        match reduce_schedule(&best, improve) {
            Some(reduced) => best = reduced,
            None => return Some(best),
        }
    }
}

fn schedule_cost(record: &Replay) -> (usize, usize) {
    let mut previous = None;
    let switches = record
        .steps
        .iter()
        .flatten()
        .filter(|&&task| {
            previous
                .replace(task)
                .is_some_and(|previous| previous != task)
        })
        .count();
    (record.steps.len(), switches)
}

fn reduce_schedule(record: &Replay, improve: impl Fn(&Replay) -> Option<Replay>) -> Option<Replay> {
    let mut candidate = record.clone();
    let len = record.steps.len();
    let mut chunk = len;
    loop {
        for start in (0..len).step_by(chunk) {
            candidate.steps.clone_from(&record.steps);
            candidate.steps.drain(start..len.min(start + chunk));
            if let Some(reduced) = improve(&candidate) {
                return Some(reduced);
            }
        }
        if chunk == 1 {
            break;
        }
        chunk = chunk.div_ceil(2);
    }

    // Keep the previous task running across a recorded switch when possible.
    let mut previous = None;
    for (index, task) in record.steps.iter().enumerate() {
        let Some(task) = *task else { continue };
        let Some(previous) = previous.replace(task) else {
            continue;
        };
        if previous == task {
            continue;
        }
        candidate.steps.clone_from(&record.steps);
        if let Some(choice) = candidate.steps.get_mut(index) {
            *choice = Some(previous);
        }
        if let Some(reduced) = improve(&candidate) {
            return Some(reduced);
        }
    }
    None
}
