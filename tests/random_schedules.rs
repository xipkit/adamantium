#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations leave each execution through a shared log"
)]

use adamantium::{
    Backoff, Cancellation, Restarts, ScopeError, Tasks, random,
    testing::{self, Config, FailureKind, Replay},
    time,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

const SECOND: Duration = Duration::from_secs(1);

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

/// The waits between the attempts of a retry that never succeeds.
async fn retry_waits(backoff: Backoff) -> Vec<Duration> {
    let mut starts = Vec::new();
    let result = adamantium::retry(&Cancellation::new(), None, backoff, || {
        starts.push(time::now());
        async { Err::<(), _>("no") }
    })
    .await;
    assert_eq!(result, Err(ScopeError::Failed("no")));
    starts.windows(2).map(|pair| pair[1] - pair[0]).collect()
}

fn jittered() -> Backoff {
    Backoff::exponential(SECOND, SECOND * 16)
        .attempts(9)
        .jitter()
}

/// The delays `jittered` would wait without jitter.
const DELAYS: [u64; 8] = [1, 2, 4, 8, 16, 16, 16, 16];

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

#[test]
fn jittered_retry_waits_stay_between_half_and_all_of_each_delay() {
    let observed = log();
    testing::check(config(), logging(&observed, || retry_waits(jittered()))).unwrap();
    let observed = entries(&observed);
    for waits in &observed {
        assert_eq!(waits.len(), DELAYS.len());
        for (wait, delay) in waits.iter().zip(DELAYS) {
            let delay = SECOND * u32::try_from(delay).unwrap();
            assert!(
                delay / 2 <= *wait && *wait <= delay,
                "{wait:?} for {delay:?}"
            );
        }
    }
    // Callers that fail together retry apart.
    let mut distinct = observed.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), observed.len());
}

#[test]
fn a_saved_record_replays_the_whole_jittered_delay_sequence() {
    let observed = log();
    let factory = logging(&observed, || retry_waits(jittered()));
    let record = testing::check(config(), factory).unwrap();
    assert_eq!(random_steps(&record), DELAYS.len());
    let path = std::env::temp_dir().join(format!(
        "adamantium-jitter-replay-{}.json",
        std::process::id()
    ));
    record.save(&path).unwrap();
    let loaded = Replay::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(loaded, record);
    let replayed = log();
    let factory = logging(&replayed, || retry_waits(jittered()));
    assert_eq!(testing::replay(&loaded, factory).unwrap(), record);
    assert_eq!(entries(&replayed), [last(&observed)]);
}

#[test]
fn backoff_without_jitter_waits_each_delay_and_draws_nothing() {
    let observed = log();
    let plain = Backoff::exponential(SECOND, SECOND * 16).attempts(9);
    let record = testing::check(config(), logging(&observed, move || retry_waits(plain))).unwrap();
    let expected = DELAYS.map(|delay| SECOND * u32::try_from(delay).unwrap());
    assert!(entries(&observed).iter().all(|waits| *waits == expected));
    assert_eq!(random_steps(&record), 0);
}

#[test]
fn jittered_restarts_wait_between_half_and_all_of_each_delay() {
    let observed = log();
    let factory = logging(&observed, || async {
        let backoff = Backoff::exponential(SECOND, SECOND * 4).jitter();
        let restarts = Restarts::new(4, Duration::from_secs(3600)).backoff(backoff);
        let result = Tasks::run(async |tasks| {
            let mut starts = Vec::new();
            let worker = tasks.spawn_supervised(restarts, move |_| {
                starts.push(time::now());
                let failed = starts.len() <= 4;
                let waits = starts
                    .windows(2)
                    .map(|pair| pair[1] - pair[0])
                    .collect::<Vec<_>>();
                async move { if failed { Err(()) } else { Ok(waits) } }
            });
            tasks.value(worker).await
        })
        .await;
        result.unwrap()
    });
    let record = testing::check(config(), factory).unwrap();
    assert_eq!(random_steps(&record), 4);
    for waits in entries(&observed) {
        assert_eq!(waits.len(), 4);
        for (wait, delay) in waits.iter().zip([1, 2, 4, 4]) {
            let delay = SECOND * delay;
            assert!(
                delay / 2 <= *wait && *wait <= delay,
                "{wait:?} for {delay:?}"
            );
        }
    }
}
