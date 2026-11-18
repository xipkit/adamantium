#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations leave each execution through a shared log"
)]

use adamantium::{
    random,
    testing::{self, Config, FailureKind, Replay},
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

fn config() -> Config {
    Config {
        seed: 203,
        iterations: 50,
        max_steps: 10_000,
    }
}

/// What each execution of a factory observed, in execution order.
type Log<T> = Arc<Mutex<Vec<T>>>;

fn log<T>() -> Log<T> {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries<T: Clone>(log: &Log<T>) -> Vec<T> {
    log.lock().unwrap().clone()
}

fn last<T: Clone>(log: &Log<T>) -> T {
    log.lock().unwrap().last().unwrap().clone()
}

/// A factory that records `scenario`'s output in `log` for every execution.
fn logging<T, F, Fut>(
    log: &Log<T>,
    scenario: F,
) -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = ()>>> + Clone + Send + Sync + 'static
where
    T: Send + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = T> + 'static,
{
    let log = Arc::clone(log);
    move || {
        let (log, scenario) = (Arc::clone(&log), scenario.clone());
        Box::pin(async move {
            let value = scenario().await;
            log.lock().unwrap().push(value);
        })
    }
}

async fn draws() -> Vec<u64> {
    let mut draws = (0..4).map(|_| random::u64()).collect::<Vec<_>>();
    draws.extend((0..4).map(|_| random::up_to(9)));
    draws
}

/// How many random draws a record made, read from its saved form.
fn random_steps(record: &Replay) -> usize {
    let json = serde_json::to_value(record).unwrap();
    json["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step.is_null())
        .count()
}

#[test]
fn draws_repeat_for_a_seed_and_differ_between_executions() {
    let first = log();
    let record = testing::check(config(), logging(&first, draws)).unwrap();
    let again = log();
    assert_eq!(
        testing::check(config(), logging(&again, draws)).unwrap(),
        record
    );
    assert_eq!(entries(&first), entries(&again));
    let first = entries(&first);
    assert_eq!(first.len(), 50);
    for draws in &first {
        assert!(draws[4..].iter().all(|&draw| draw <= 9));
    }
    let mut distinct = first.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), first.len());
    assert_eq!(random_steps(&record), 8);
}

#[test]
fn a_replay_repeats_the_draws_of_its_execution() {
    let observed = log();
    let record = testing::check(config(), logging(&observed, draws)).unwrap();
    let replayed = log();
    assert_eq!(
        testing::replay(&record, logging(&replayed, draws)).unwrap(),
        record
    );
    assert_eq!(entries(&replayed), [last(&observed)]);
}

#[test]
fn a_replay_that_draws_differently_is_a_mismatch() {
    let record = testing::check(config(), || async {
        random::u64();
    })
    .unwrap();
    for (scenario, draws) in [(0, "fewer"), (2, "more")] {
        let mismatch = testing::replay(&record, move || async move {
            for _ in 0..scenario {
                random::u64();
            }
        })
        .unwrap_err();
        assert_eq!(
            mismatch.kind(),
            FailureKind::ReplayMismatch,
            "{draws} draws"
        );
    }
}
