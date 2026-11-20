#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "test observations, schedule assertions and deliberate execution-boundary misuse"
)]

use std::{
    collections::BTreeMap,
    future::Future,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use adamantium::{
    Actor, Backoff, Cancellation, Tasks, retry_if,
    testing::{self, Config, FailureKind, FaultInjector, FaultRule, FaultStep, Latency, Replay},
    time,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreError {
    Unavailable,
    RateLimited,
    Timeout,
}

trait KeyValueStore: Send + Sync {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, StoreError>> + Send + 'a>>;

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), StoreError>> + Send + 'a>>;
}

#[derive(Clone)]
struct MockKvStore {
    data: Actor<BTreeMap<String, String>>,
    faults: FaultInjector<StoreError>,
}

impl MockKvStore {
    fn new(faults: FaultInjector<StoreError>) -> Self {
        Self {
            data: Actor::new(BTreeMap::new()),
            faults,
        }
    }
}

impl KeyValueStore for MockKvStore {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, StoreError>> + Send + 'a>> {
        Box::pin(async move {
            self.faults.inject("get").await?;
            Ok(self.data.read(|map| map.get(key).cloned()))
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), StoreError>> + Send + 'a>> {
        Box::pin(async move {
            self.faults.inject("put").await?;
            self.data.with(|map| {
                map.insert(key.to_owned(), value.to_owned());
            });
            Ok(())
        })
    }
}

fn config() -> Config {
    Config {
        seed: 42,
        iterations: 100,
        max_steps: 20_000,
        ..Config::default()
    }
}

#[test]
fn mocked_async_trait_failure_is_discovered_and_replayed_identically() {
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Observation {
        elapsed: Duration,
        put_calls: usize,
        get_calls: usize,
        outcome: Result<Option<String>, StoreError>,
    }

    let observations: Arc<Mutex<Vec<Observation>>> = Arc::new(Mutex::new(Vec::new()));

    let make_scenario = |log: Arc<Mutex<Vec<Observation>>>| {
        move || {
            let log = Arc::clone(&log);
            async move {
                let faults = FaultInjector::new()
                    .with_rule(
                        "put",
                        FaultRule::fail(0.25, StoreError::Unavailable).with_latency_range(
                            Duration::from_millis(5),
                            Duration::from_millis(25),
                        ),
                    )
                    .with_rule(
                        "get",
                        FaultRule::delay(Latency::fixed(Duration::from_millis(10))),
                    );
                let store = MockKvStore::new(faults.clone());
                let start = time::now();

                // Retry once on Unavailable; if two consecutive puts fail, the write fails.
                let cancel = Cancellation::new();
                let write_res = retry_if(
                    &cancel,
                    None,
                    Backoff::constant(Duration::from_millis(15)).attempts(2),
                    || store.put("alpha", "1"),
                    |err| *err == StoreError::Unavailable,
                )
                .await;

                let outcome = match write_res {
                    Ok(()) => store.get("alpha").await,
                    Err(adamantium::ScopeError::Failed(err)) => Err(err),
                    Err(_) => Err(StoreError::Timeout),
                };

                log.lock().unwrap().push(Observation {
                    elapsed: time::now().saturating_duration_since(start),
                    put_calls: faults.calls("put"),
                    get_calls: faults.calls("get"),
                    outcome: outcome.clone(),
                });

                assert_eq!(outcome, Ok(Some("1".to_owned())));
            }
        }
    };

    let failure = testing::check(config(), make_scenario(Arc::clone(&observations)))
        .expect_err("two consecutive probabilistic put faults should fail within 100 iterations");
    assert_eq!(failure.kind(), FailureKind::Panic);

    let first_failure_obs = observations.lock().unwrap().last().unwrap().clone();
    assert_eq!(first_failure_obs.outcome, Err(StoreError::Unavailable));
    assert_eq!(first_failure_obs.put_calls, 2);
    assert_eq!(first_failure_obs.get_calls, 0);

    // Replaying the discovered failure reproduces the exact same Replay and Observation.
    let replayed_obs = Arc::new(Mutex::new(Vec::new()));
    let replayed = testing::replay(failure.replay(), make_scenario(Arc::clone(&replayed_obs)))
        .expect_err("replay must reproduce the discovered failure");
    assert_eq!(replayed.kind(), FailureKind::Panic);
    assert_eq!(replayed.replay(), failure.replay());
    assert_eq!(
        replayed_obs.lock().unwrap().as_slice(),
        &[first_failure_obs]
    );

    // Re-running check with the same seed discovers the exact same Replay.
    let again_obs = Arc::new(Mutex::new(Vec::new()));
    let again = testing::check(config(), make_scenario(Arc::clone(&again_obs))).unwrap_err();
    assert_eq!(again.replay(), failure.replay());
}

#[test]
fn passing_run_with_latency_and_retries_replays_identical_virtual_timestamps() {
    let observed: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    let make_scenario = |log: Arc<Mutex<Vec<Duration>>>| {
        move || {
            let log = Arc::clone(&log);
            async move {
                let faults = FaultInjector::new().with_rule(
                    "put",
                    FaultRule::fail(0.3, StoreError::Unavailable)
                        .with_latency_range(Duration::from_millis(10), Duration::from_millis(40)),
                );
                let store = MockKvStore::new(faults);
                let start = time::now();
                let cancel = Cancellation::new();

                // With 12 attempts, 0.3^12 is ~5e-7, so all iterations succeed.
                let (s1, c1) = (store.clone(), cancel.clone());
                let (s2, c2) = (store.clone(), cancel.clone());
                Tasks::run(async |tasks: &mut Tasks<()>| {
                    tasks.spawn(async move {
                        retry_if(
                            &c1,
                            None,
                            Backoff::constant(Duration::from_millis(5)).attempts(12),
                            || s1.put("k1", "v1"),
                            |err| *err == StoreError::Unavailable,
                        )
                        .await
                        .unwrap();
                        Ok(())
                    });

                    tasks.spawn(async move {
                        retry_if(
                            &c2,
                            None,
                            Backoff::constant(Duration::from_millis(5)).attempts(12),
                            || s2.put("k2", "v2"),
                            |err| *err == StoreError::Unavailable,
                        )
                        .await
                        .unwrap();
                        Ok(())
                    });
                    Ok::<(), ()>(())
                })
                .await
                .unwrap();

                assert_eq!(store.get("k1").await, Ok(Some("v1".to_owned())));
                assert_eq!(store.get("k2").await, Ok(Some("v2".to_owned())));
                log.lock()
                    .unwrap()
                    .push(time::now().saturating_duration_since(start));
            }
        }
    };

    let wall_start = std::time::Instant::now();
    let record: Replay = testing::check(config(), make_scenario(Arc::clone(&observed))).unwrap();
    assert!(wall_start.elapsed() < Duration::from_secs(5));

    let replayed: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(
        testing::replay(&record, make_scenario(Arc::clone(&replayed))).unwrap(),
        record
    );
    assert_eq!(
        replayed.lock().unwrap().as_slice(),
        &[observed.lock().unwrap().last().copied().unwrap()]
    );
}

#[test]
fn scripted_schedule_precedence_and_fallback_to_rules() {
    let check_cfg = Config {
        iterations: 10,
        ..config()
    };

    testing::check(check_cfg, || async {
        let faults = FaultInjector::new()
            .with_op_schedule(
                "put",
                [
                    FaultStep::fail(StoreError::RateLimited),
                    FaultStep::delay_then_fail(Duration::from_secs(2), StoreError::Timeout),
                ],
            )
            .with_schedule([FaultStep::delay(Duration::from_secs(1))])
            .with_rule(
                "put",
                FaultRule::fail_when(|call| call == 4, StoreError::Unavailable),
            )
            .with_default_rule(FaultRule::delay(Latency::fixed(Duration::from_millis(50))));

        let store = MockKvStore::new(faults.clone());
        let start = time::now();

        // 1st put: op schedule step 1 -> immediate RateLimited
        assert_eq!(store.put("a", "1").await, Err(StoreError::RateLimited));
        assert_eq!(time::now().saturating_duration_since(start), Duration::ZERO);

        // 2nd put: op schedule step 2 -> 2s delay then Timeout
        assert_eq!(store.put("a", "1").await, Err(StoreError::Timeout));
        assert_eq!(
            time::now().saturating_duration_since(start),
            Duration::from_secs(2)
        );

        // 1st get: no op schedule for "get", consumes global schedule -> 1s delay, Ok
        assert_eq!(store.get("a").await, Ok(None));
        assert_eq!(
            time::now().saturating_duration_since(start),
            Duration::from_secs(3)
        );

        // 3rd put: op & global schedules exhausted -> uses "put" rule (call 3 != 4) -> Ok immediately
        assert_eq!(store.put("a", "1").await, Ok(()));
        assert_eq!(
            time::now().saturating_duration_since(start),
            Duration::from_secs(3)
        );

        // 4th put: "put" rule condition (call == 4) matches -> Unavailable
        assert_eq!(store.put("a", "2").await, Err(StoreError::Unavailable));

        // 2nd get: falls back to default rule -> 50ms delay, Ok(Some("1"))
        assert_eq!(store.get("a").await, Ok(Some("1".to_owned())));
        assert_eq!(
            time::now().saturating_duration_since(start),
            Duration::from_secs(3) + Duration::from_millis(50)
        );

        assert_eq!(faults.calls("put"), 4);
        assert_eq!(faults.calls("get"), 2);
        assert_eq!(faults.total_calls(), 6);
    })
    .unwrap();
}

#[test]
fn non_clone_error_factory_works_with_rules_and_steps() {
    let check_cfg = Config {
        iterations: 5,
        ..config()
    };

    testing::check(check_cfg, || async {
        let faults: FaultInjector<io::Error> = FaultInjector::new()
            .with_op_schedule(
                "connect",
                [FaultStep::fail_with(|| {
                    io::Error::new(io::ErrorKind::ConnectionRefused, "refused")
                })],
            )
            .with_rule(
                "read",
                FaultRule::fail_when_with(
                    |call| call == 2,
                    || io::Error::new(io::ErrorKind::TimedOut, "timed out"),
                ),
            );

        let err = faults.inject("connect").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);

        assert!(faults.inject("read").await.is_ok());
        let err = faults.inject("read").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    })
    .unwrap();
}

#[test]
fn foreign_execution_use_is_rejected() {
    let escaped: Arc<Mutex<Option<FaultInjector<StoreError>>>> = Arc::new(Mutex::new(None));
    let escaped_slot = Arc::clone(&escaped);

    let cfg = Config {
        iterations: 1,
        ..config()
    };

    testing::check(cfg, move || {
        let slot = Arc::clone(&escaped_slot);
        async move {
            *slot.lock().unwrap() = Some(FaultInjector::new());
        }
    })
    .unwrap();

    let injector = escaped.lock().unwrap().take().unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = injector.total_calls();
        drop(injector);
    }));
    assert!(result.is_err());
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn works_outside_deterministic_harness_with_tokio_time() {
    let faults = FaultInjector::new()
        .with_op_schedule(
            "fetch",
            [FaultStep::delay_then_fail(
                Duration::from_millis(25),
                StoreError::Timeout,
            )],
        )
        .with_rule(
            "fetch",
            FaultRule::delay(Latency::fixed(Duration::from_millis(10))),
        );

    let start = time::now();
    assert_eq!(faults.inject("fetch").await, Err(StoreError::Timeout));
    assert_eq!(
        time::now().saturating_duration_since(start),
        Duration::from_millis(25)
    );

    assert_eq!(faults.inject("fetch").await, Ok(()));
    assert_eq!(
        time::now().saturating_duration_since(start),
        Duration::from_millis(35)
    );
}
