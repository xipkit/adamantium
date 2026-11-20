#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations and deliberate foreign-runtime misuse"
)]

use adamantium::{
    Actor, Cancellation, Channel, Reply, Tasks, TryReceiveError, Watch,
    testing::{
        self, Config, CorpusError, DfsStatus, FailureKind, Replay, SchedulerMode, TimelineEventKind,
    },
    time,
};
use std::{
    future::pending,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const MODES: [SchedulerMode; 3] = [
    SchedulerMode::Random,
    SchedulerMode::Pct { depth: 3 },
    SchedulerMode::Dfs,
];

fn config() -> Config {
    Config {
        seed: 143,
        iterations: 100,
        max_steps: 2_000,
        ..Config::default()
    }
}

fn race_config() -> Config {
    Config {
        iterations: 2_000,
        ..config()
    }
}

async fn split_increment() -> usize {
    let count = Actor::new(0);
    let workers = (0..2)
        .map(|_| {
            let count = count.clone();
            async move {
                let previous = count.read(|n| *n);
                count.with(|n| *n = previous + 1);
            }
        })
        .collect::<Vec<_>>();
    testing::parallel(workers).await;
    count.snapshot()
}

async fn broken_increment() {
    assert_eq!(split_increment().await, 2, "lost update");
}

#[test]
fn finds_and_replays_a_race_without_awaits() {
    for mode in MODES {
        let config = race_config();
        let exploration = config.with_scheduler(mode);
        let failure = testing::check(exploration, broken_increment).unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Panic);
        assert_eq!(failure.replay().scheduler(), mode);
        assert_eq!(failure.replay().dfs_status(), None);
        assert_eq!(failure.replay().iterations(), config.iterations);
        assert_eq!(failure.replay().max_steps(), config.max_steps);
        assert!(failure.replay().iteration() <= config.iterations);
        let again = testing::replay(failure.replay(), broken_increment).unwrap_err();
        assert_eq!(again.kind(), FailureKind::Panic);
        assert_eq!(failure.replay(), again.replay());
        let same_exploration = testing::check(exploration, broken_increment).unwrap_err();
        assert_eq!(failure.replay(), same_exploration.replay());
    }
}

async fn two_workers() {
    let count = Actor::new(0);
    let worker = count.clone();
    let child = shuttle::future::spawn(async move { worker.with(|n| *n += 1) });
    count.with(|n| *n += 1);
    child.await.unwrap();
    assert_eq!(count.snapshot(), 2);
}

#[test]
fn dfs_distinguishes_a_complete_tree_from_the_iteration_limit() {
    let dfs = |iterations| {
        Config {
            iterations,
            ..config()
        }
        .with_scheduler(SchedulerMode::Dfs)
    };
    let last = testing::check(dfs(100), two_workers).unwrap();
    assert!(last.iteration() > 1);
    assert!(last.iteration() < last.iterations());
    assert_eq!(last.dfs_status(), Some(DfsStatus::Complete));
    let exact = testing::check(dfs(last.iteration()), two_workers).unwrap();
    assert_eq!(exact.iteration(), last.iteration());
    assert_eq!(exact.dfs_status(), Some(DfsStatus::Complete));

    let limited = testing::check(dfs(last.iteration() - 1), two_workers).unwrap();
    assert_eq!(limited.dfs_status(), Some(DfsStatus::IterationLimit));
    assert_eq!(limited.iteration(), last.iteration() - 1);
    assert_eq!(testing::replay(&limited, two_workers).unwrap(), limited);
}

#[test]
fn pct_finishes_calibration_without_task_choices() {
    let exploration = Config {
        iterations: 5,
        ..config()
    }
    .with_scheduler(SchedulerMode::Pct { depth: 1 });
    let record = testing::check(exploration, || async {}).unwrap();
    assert_eq!(record.iteration(), 1);
    assert_eq!(testing::replay(&record, || async {}).unwrap(), record);

    let record = testing::check(exploration, two_workers).unwrap();
    assert_eq!(record.iteration(), 5);
    assert_eq!(testing::replay(&record, two_workers).unwrap(), record);
}

async fn atomic_increment() {
    let count = Actor::new(0);
    let workers = (0..3)
        .map(|_| {
            let count = count.clone();
            async move {
                for _ in 0..3 {
                    count.with(|n| *n += 1);
                }
            }
        })
        .collect::<Vec<_>>();
    testing::parallel(workers).await;
    assert_eq!(count.snapshot(), 9);
    assert!(!count.is_shared());
    assert_eq!(count.into_inner().ok(), Some(9));
}

#[test]
fn atomic_updates_pass_and_successful_schedules_replay() {
    let record = testing::check(config(), atomic_increment).unwrap();
    let same_seed = testing::check(config(), atomic_increment).unwrap();
    assert_eq!(record, same_seed);
    assert_eq!(record.iteration(), 100);
    assert_eq!(testing::replay(&record, atomic_increment).unwrap(), record);
}

#[test]
fn nested_calls_poison_recovery_and_debug_keep_their_contract() {
    testing::check(config(), || async {
        let actor = Actor::new(0);
        assert!(catch_unwind(AssertUnwindSafe(|| actor.with(|_| actor.read(|_| ())))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| actor.with(|n| {
                *n = 7;
                panic!("method failed");
            })))
            .is_err()
        );
        actor.with(|n| *n += 1);
        assert_eq!(actor.snapshot(), 8);
        let other = Actor::new(4);
        assert!(catch_unwind(AssertUnwindSafe(|| actor.with(|_| other.with(|_| ())))).is_err());
        assert_eq!(other.snapshot(), 4);
        let debug = Actor::new(5);
        assert_eq!(format!("{debug:?}"), "Actor(5)");
        debug.with(|_| assert_eq!(format!("{debug:?}"), "Actor(<in use>)"));
    })
    .unwrap();
}

#[test]
fn ownership_observations_are_scheduling_points() {
    let seen = Arc::new(Mutex::new([false; 2]));
    let observed = seen.clone();
    testing::check(config(), move || {
        let observed = observed.clone();
        async move {
            let actor = Actor::new(0);
            let other = actor.clone();
            let workers: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = vec![
                Box::pin(async move { drop(other); }),
                Box::pin(async move {
                    let shared = actor.is_shared();
                    observed.lock().unwrap()[usize::from(shared)] = true;
                }),
            ];
            testing::parallel(workers).await;
        }
    }).unwrap();
    assert_eq!(*seen.lock().unwrap(), [true, true]);
}

#[test]
fn step_exhaustion_and_deadlock_are_distinct_failures_and_replay() {
    async fn spin() {
        let actor = Actor::new(0);
        loop {
            actor.with(|n| *n += 1);
        }
    }
    for mode in MODES {
        let exploration = Config {
            max_steps: 20,
            ..config()
        }
        .with_scheduler(mode);
        let failure = testing::check(exploration, spin).unwrap_err();
        assert_eq!(failure.kind(), FailureKind::StepLimit);
        let again = testing::replay(failure.replay(), spin).unwrap_err();
        assert_eq!(again.kind(), FailureKind::StepLimit);
        assert_eq!(again.replay(), failure.replay());
    }
    let failure = testing::check(config(), pending::<()>).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Deadlock);
    assert_eq!(
        testing::replay(failure.replay(), pending::<()>)
            .unwrap_err()
            .kind(),
        FailureKind::Deadlock
    );
    assert_eq!(Actor::new(7).snapshot(), 7);
    testing::check(config(), atomic_increment).unwrap();
}

#[test]
fn rejects_production_actor_in_harness() {
    let actor = Arc::new(Actor::new(1));
    let failure = testing::check(config(), move || {
        let actor = actor.clone();
        async move {
            actor.with(|n| *n += 1);
        }
    })
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
}

#[test]
fn rejects_model_actor_outside_its_execution_including_drop() {
    let escaped = Arc::new(Mutex::new(None));
    let out = escaped.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let out = out.clone();
            async move {
                *out.lock().unwrap() = Some(Actor::new(7));
            }
        },
    )
    .unwrap();
    let actor = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| actor.snapshot())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(actor))).is_err());
}

#[test]
fn rejects_actor_reused_between_iterations() {
    let held = Arc::new(Mutex::new(None));
    let retained = held.clone();
    let failure = testing::check(config(), move || {
        let retained = retained.clone();
        async move {
            let mut retained = retained.lock().unwrap();
            match &*retained {
                Some(actor) => {
                    let _: i32 = Actor::snapshot(actor);
                }
                None => {
                    *retained = Some(Actor::new(7));
                }
            }
        }
    })
    .unwrap_err();
    assert_eq!(failure.replay().iteration(), 2);
    let actor = held.lock().unwrap_or_else(|p| p.into_inner()).take();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(actor))).is_err());
}

#[test]
fn supported_primitives_select_the_model_and_reject_foreign_channels() {
    testing::check(config(), || async {
        let _tasks = Tasks::<()>::new();
        let _cancel = Cancellation::new();
        assert_eq!(adamantium::blocking(|| 42).await, 42);
    })
    .unwrap();
    #[cfg(all(feature = "signal", any(unix, windows)))]
    assert_eq!(
        testing::check(
            Config {
                iterations: 1,
                ..config()
            },
            || async {
                let cancel = Cancellation::new();
                cancel.cancel();
                cancel.cancel_on_signal().await.unwrap();
            },
        )
        .unwrap_err()
        .kind(),
        FailureKind::Panic
    );
    let channel = Arc::new(Channel::bounded(1));
    assert!(
        testing::check(config(), move || {
            let channel = channel.clone();
            async move {
                channel.try_send(1).unwrap();
            }
        })
        .is_err()
    );
}

#[test]
fn named_tasks_reject_the_harness_before_converting_names() {
    struct Name(Arc<Mutex<bool>>);

    impl From<Name> for adamantium::Str {
        fn from(name: Name) -> Self {
            *name.0.lock().unwrap() = true;
            "foreign task".into()
        }
    }

    for blocking in [false, true] {
        let tasks = Arc::new(Mutex::new(Tasks::<()>::new()));
        let converted = Arc::new(Mutex::new(false));
        let observed = converted.clone();
        let failure = testing::check(config(), move || {
            let tasks = tasks.clone();
            let name = Name(converted.clone());
            async move {
                let mut tasks = tasks.lock().unwrap();
                if blocking {
                    tasks.spawn_blocking_named(name, || Ok(()));
                } else {
                    tasks.spawn_named(name, async { Ok(()) });
                }
            }
        })
        .unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Panic);
        assert!(!*observed.lock().unwrap());
    }
}

#[tokio::test]
async fn feature_unification_preserves_production_and_rejects_nested_runtime() {
    let actor = Actor::new(0);
    let channel = Channel::bounded(1);
    channel.send(7).await.unwrap();
    assert_eq!(channel.receive().await, Some(7));
    Tasks::<()>::run(async |tasks| {
        let worker = actor.clone();
        tasks.spawn(async move {
            worker.with(|n| *n += 1);
            Ok(())
        });
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(actor.snapshot(), 1);
    assert!(catch_unwind(|| testing::check(config(), || async {})).is_err());
}

#[test]
fn nesting_harnesses_and_zero_bounds_are_errors() {
    assert!(
        testing::check(config(), || async {
            testing::check(config(), || async {}).unwrap();
        })
        .is_err()
    );
    assert!(
        catch_unwind(|| testing::check(
            Config {
                iterations: 0,
                ..config()
            },
            || async {}
        ))
        .is_err()
    );
    assert!(
        catch_unwind(|| testing::check(
            Config {
                max_steps: 0,
                ..config()
            },
            || async {}
        ))
        .is_err()
    );
    assert!(
        catch_unwind(|| testing::check(
            config().with_scheduler(SchedulerMode::Pct { depth: 0 }),
            || async {}
        ))
        .is_err()
    );
    assert!(
        catch_unwind(|| testing::check(
            Config {
                wall_epoch: std::time::UNIX_EPOCH - Duration::from_nanos(1),
                ..config()
            },
            || async {}
        ))
        .is_err()
    );
    testing::check(config(), || async {}).unwrap();
}

fn replay_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "adamantium-replay-{}-{name}.json",
        std::process::id()
    ))
}

#[test]
fn trace_files_round_trip_without_payloads_and_validate_versions() {
    let failure = testing::check(config(), || async {
        panic!("private fixture payload");
    })
    .unwrap_err();
    let path = replay_path("metadata");
    failure.replay().save(&path).unwrap();
    let json = std::fs::read_to_string(&path).unwrap();
    assert!(!json.contains("private fixture payload"));
    assert_eq!(Replay::load(&path).unwrap(), *failure.replay());
    let record: serde_json::Value = serde_json::from_str(&json).unwrap();
    let assert_invalid = |record: serde_json::Value, message: &str| {
        std::fs::write(&path, record.to_string()).unwrap();
        let error = Replay::load(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(message), "{error}");
    };

    let mut old = record.as_object().unwrap().clone();
    old.insert("format".into(), 2.into());
    old.remove("scheduler");
    old.remove("iterations");
    assert_invalid(old.into(), "format is 2, expected 4");

    let mut missing_epoch = record.as_object().unwrap().clone();
    missing_epoch.remove("wall_epoch");
    assert_invalid(missing_epoch.into(), "missing field `wall_epoch`");

    let cases = [
        ("engine", "shuttle=0.0.0".into(), "engine is"),
        ("iterations", 0.into(), "iterations must be nonzero"),
        ("iteration", 0.into(), "iteration is 0, expected 1..="),
        ("iteration", 101.into(), "iteration is 101, expected 1..="),
        ("max_steps", 0.into(), "max_steps must be nonzero"),
        (
            "steps",
            serde_json::json!([]),
            "steps must start with a task choice",
        ),
        (
            "dfs_status",
            "complete".into(),
            "dfs_status requires the DFS scheduler",
        ),
        (
            "scheduler",
            "unknown_mode".into(),
            "scheduler: unknown variant `unknown_mode`",
        ),
        (
            "scheduler",
            serde_json::json!({"pct": {"depth": 0}}),
            "scheduler.pct.depth must be nonzero",
        ),
    ];
    for (field, value, message) in cases {
        let mut invalid = record.as_object().unwrap().clone();
        invalid.insert(field.into(), value);
        assert_invalid(invalid.into(), message);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn corpus_replays_saved_failures_and_identifies_the_first_error() {
    let paths = MODES.map(|mode| {
        let failure =
            testing::check(race_config().with_scheduler(mode), broken_increment).unwrap_err();
        let path = replay_path(&format!("corpus-{mode:?}"));
        failure.replay().save(&path).unwrap();
        assert_eq!(Replay::load(&path).unwrap(), *failure.replay());
        path
    });
    let [first, second, ..] = &paths;
    let error = testing::replay_corpus(&paths, broken_increment).unwrap_err();
    assert!(matches!(error, CorpusError::Execution { path, source }
        if &path == first && source.kind() == FailureKind::Panic));

    let error = testing::replay_corpus(&paths, || async {}).unwrap_err();
    assert!(matches!(error, CorpusError::Execution { path, source }
        if &path == first && source.kind() == FailureKind::ReplayMismatch));

    let visits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&visits);
    let factory = move || {
        observed.fetch_add(1, Ordering::Relaxed);
        async { assert_eq!(split_increment().await, 1) }
    };
    testing::replay_corpus(&paths, factory.clone()).unwrap();
    assert_eq!(visits.swap(0, Ordering::Relaxed), paths.len());

    std::fs::write(second, "{\"format\":2}").unwrap();
    let error = testing::replay_corpus(&paths, factory).unwrap_err();
    assert!(matches!(error, CorpusError::Load { path, source }
        if &path == second && source.to_string().contains("format is 2, expected 4")));
    assert_eq!(visits.load(Ordering::Relaxed), 1);
    for path in paths {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn dfs_uses_fixed_random_data_and_replay_detects_an_extra_draw() {
    use shuttle::rand::RngCore;

    let data = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&data);
    let factory = move || {
        let observed = Arc::clone(&observed);
        async move {
            two_workers().await;
            let value = shuttle::rand::thread_rng().next_u64();
            observed.lock().unwrap().push(value);
        }
    };
    let record =
        testing::check(config().with_scheduler(SchedulerMode::Dfs), factory.clone()).unwrap();
    assert!(record.iteration() > 1);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
    let draws = data.lock().unwrap();
    assert_eq!(draws.len(), record.iteration() + 1);
    assert!(draws.windows(2).all(|pair| pair.first() == pair.last()));

    let mismatch = testing::replay(&record, || async {
        two_workers().await;
        let _first = shuttle::rand::thread_rng().next_u64();
        let _extra = shuttle::rand::thread_rng().next_u64();
    })
    .unwrap_err();
    assert_eq!(mismatch.kind(), FailureKind::ReplayMismatch);
}

#[test]
fn replay_detects_changed_scenario() {
    let record = testing::check(config(), atomic_increment).unwrap();
    assert_eq!(
        testing::replay(&record, || async {}).unwrap_err().kind(),
        FailureKind::ReplayMismatch
    );
}

#[test]
fn replay_rejects_early_panics_and_reports_the_observed_trace() {
    let successful = testing::check(config(), atomic_increment).unwrap();
    let failed = testing::check(config(), broken_increment).unwrap_err();
    for original in [&successful, failed.replay()] {
        let mismatch = testing::replay(original, || async {
            panic!("unrelated early failure");
        })
        .unwrap_err();
        assert_eq!(mismatch.kind(), FailureKind::ReplayMismatch);
        assert_ne!(mismatch.replay(), original);
        assert_eq!(mismatch.replay().seed(), original.seed());
        assert_eq!(mismatch.replay().iteration(), original.iteration());
        let reproduced = testing::replay(mismatch.replay(), || async {
            panic!("unrelated early failure");
        })
        .unwrap_err();
        assert_eq!(reproduced.kind(), FailureKind::Panic);
        assert_eq!(reproduced.replay(), mismatch.replay());
    }
}

#[test]
fn prepolled_channel_future_is_rejected_before_receiving() {
    use futures_util::FutureExt;
    let channel = Arc::new(Channel::bounded(1));
    let reader = channel.clone();
    let mut future = Box::pin(async move {
        reader.receive().await;
    });
    assert!(future.as_mut().now_or_never().is_none());
    channel.try_send(42).unwrap();
    let future = Mutex::new(Some(future));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || future.lock().unwrap().take().unwrap(),
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(channel.try_receive(), Ok(42));
}

#[test]
fn prepolled_task_scope_is_rejected_and_cleans_up_without_double_panic() {
    use futures_util::FutureExt;
    let cancellation = Cancellation::new();
    let signal = cancellation.clone();
    let mut future = Box::pin(async move {
        Tasks::<()>::run_until(signal, None, async |_| pending::<Result<(), ()>>().await)
            .await
            .unwrap();
    });
    assert!(future.as_mut().now_or_never().is_none());
    let future = Mutex::new(Some(future));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || future.lock().unwrap().take().unwrap(),
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert!(cancellation.is_cancelled());
}

#[test]
fn dropping_foreign_task_and_cancellation_handles_is_rejected() {
    let signal = Mutex::new(Some(Cancellation::new()));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let signal = signal.lock().unwrap().take();
            async move {
                drop(signal);
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (scope, task) = runtime.block_on(async {
        let mut scope = Tasks::<()>::new();
        let task = scope.spawn(pending::<Result<(), ()>>());
        (scope, task)
    });
    let task = Mutex::new(Some(task));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let task = task.lock().unwrap().take();
            async move {
                drop(task);
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    drop(scope);
}

#[test]
fn foreign_receiver_stream_cannot_bypass_the_harness_boundary() {
    use futures_util::StreamExt;
    let (sender, receiver) = Channel::bounded(1).split();
    sender.try_send(42).unwrap();
    let worker = Mutex::new(Some(receiver.clone()));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let reader = worker.lock().unwrap().take().unwrap();
            async move {
                let mut reader = std::pin::pin!(reader);
                reader.next().await;
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(receiver.try_receive(), Ok(42));
}

#[test]
fn replies_and_watches_support_the_model_and_reject_foreign_handles() {
    fn carried_in<V: Send + 'static>(value: V, body: fn(V)) -> FailureKind {
        let value = Mutex::new(Some(value));
        testing::check(
            Config {
                iterations: 1,
                ..config()
            },
            move || {
                let value = value.lock().unwrap().take().unwrap();
                async move { body(value) }
            },
        )
        .unwrap_err()
        .kind()
    }

    testing::check(config(), || async {
        let (reply, answer) = Reply::pair();
        reply.send(42).unwrap();
        assert_eq!(answer.receive().await, Some(42));
        let watch = Watch::new(0);
        let mut watcher = watch.subscribe();
        watch.send(7).unwrap();
        assert_eq!(watcher.receive().await, Some(7));
    })
    .unwrap();

    let (reply, answer) = Reply::<String>::pair();
    assert_eq!(
        carried_in(reply, |reply| drop(reply.send(String::from("sent")))),
        FailureKind::Panic
    );
    assert_eq!(
        answer.try_receive(),
        Err(TryReceiveError::Empty),
        "nothing was sent from the harness"
    );
    assert_eq!(
        carried_in(answer, |answer| drop(answer.try_receive())),
        FailureKind::Panic
    );
    let (reply, answer) = Reply::<String>::pair();
    assert_eq!(carried_in(reply, drop), FailureKind::Panic);
    assert_eq!(carried_in(answer, drop), FailureKind::Panic);

    let watch = Watch::new(String::new());
    let watcher = watch.subscribe();
    assert_eq!(
        carried_in(watch.clone(), |watch| drop(
            watch.send(String::from("sent"))
        )),
        FailureKind::Panic
    );
    assert_eq!(
        carried_in(watcher.clone(), |mut watcher| drop(watcher.try_receive())),
        FailureKind::Panic
    );
    assert_eq!(carried_in(watcher, drop), FailureKind::Panic);
    assert_eq!(
        carried_in(watch.clone(), |watch| {
            let _held = watch;
            std::panic::panic_any("unwinding");
        }),
        FailureKind::Panic
    );
    assert_eq!(carried_in(watch.clone(), drop), FailureKind::Panic);
    assert!(!watch.is_closed(), "the rejected drop left the count alone");
    assert_eq!(watch.get(), "", "nothing was sent from the harness");

    struct SendOnDrop(Watch<String>);
    impl Drop for SendOnDrop {
        fn drop(&mut self) {
            drop(self.0.send(String::from("unwound")));
        }
    }
    assert_eq!(
        carried_in(SendOnDrop(watch.clone()), |guard| {
            let _held = guard;
            std::panic::panic_any("unwinding");
        }),
        FailureKind::Panic
    );
    assert_eq!(watch.get(), "unwound");
}

#[test]
fn prepolled_watch_future_is_rejected_before_receiving() {
    use futures_util::FutureExt;
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let mut reader = watcher.clone();
    let mut future = Box::pin(async move {
        reader.receive().await;
    });
    assert!(future.as_mut().now_or_never().is_none());
    watch.send(42).unwrap();
    let future = Mutex::new(Some(future));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || future.lock().unwrap().take().unwrap(),
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(watcher.try_receive(), Ok(42));
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Empty));
}

#[test]
fn explain_reconstructs_named_task_timeline_without_payloads_or_perturbation() {
    async fn deadlocking_pipeline() {
        let (tx, rx) = Channel::<String>::bounded(1).split();
        let watch = Watch::new(String::from("initial-watch-payload"));
        let mut watcher = watch.subscribe();
        let mut tasks = Tasks::<()>::new();

        let sender_tx = tx.clone();
        let sender_watch = watch.clone();
        tasks.spawn_named("notifier", async move {
            time::sleep(Duration::from_millis(15)).await;
            sender_tx
                .send(String::from("secret-channel-payload"))
                .await
                .unwrap();
            sender_watch
                .send(String::from("secret-watch-payload"))
                .unwrap();
            Ok(())
        });

        tasks.spawn_named("channel-reader", async move {
            let first = rx.receive().await;
            assert!(first.is_some());
            let _second = rx.receive().await;
            Ok(())
        });

        tasks.spawn_named("watch-waiter", async move {
            let first = watcher.receive().await;
            assert!(first.is_some());
            let _second = watcher.receive().await;
            Ok(())
        });

        tasks.join(Ok(())).await.unwrap();
    }

    for mode in MODES {
        let exploration = Config {
            iterations: 20,
            ..config()
        }
        .with_scheduler(mode);

        let failure = testing::check(exploration, deadlocking_pipeline).unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Deadlock);

        let replayed = testing::replay(failure.replay(), deadlocking_pipeline).unwrap_err();
        assert_eq!(replayed.kind(), FailureKind::Deadlock);
        assert_eq!(replayed.replay(), failure.replay());

        let timeline = testing::explain(failure.replay(), deadlocking_pipeline);
        assert_eq!(timeline.failure(), Some(FailureKind::Deadlock));
        assert_eq!(timeline.replay(), failure.replay());

        let task_names: Vec<&str> = timeline
            .tasks()
            .iter()
            .filter_map(|task| task.name())
            .collect();
        assert!(task_names.contains(&"notifier"), "{task_names:?}");
        assert!(task_names.contains(&"channel-reader"), "{task_names:?}");
        assert!(task_names.contains(&"watch-waiter"), "{task_names:?}");

        let saw_timer_wakeup = timeline.events().iter().any(|event| {
            event.timestamp() == Duration::from_millis(15)
                && matches!(
                    event.kind(),
                    TimelineEventKind::Wakeup {
                        operation: "time.sleep",
                        target_task_names,
                        ..
                    } if target_task_names.iter().any(|name| name.as_deref() == Some("notifier"))
                )
        });
        assert!(saw_timer_wakeup, "{timeline}");

        let saw_channel_wakeup = timeline.events().iter().any(|event| {
            matches!(
                event.kind(),
                TimelineEventKind::Wakeup {
                    by_task_name: Some(by),
                    operation: "channel.receive",
                    target_task_names,
                    ..
                } if by == "notifier"
                    && target_task_names.iter().any(|name| name.as_deref() == Some("channel-reader"))
            )
        });
        assert!(saw_channel_wakeup, "{timeline}");

        let reader_waiting = timeline.waiting().iter().any(|entry| {
            entry.task_name() == Some("channel-reader") && entry.operation() == "channel.receive"
        });
        let waiter_waiting = timeline.waiting().iter().any(|entry| {
            entry.task_name() == Some("watch-waiter") && entry.operation() == "watch.receive"
        });
        assert!(reader_waiting, "{timeline}");
        assert!(waiter_waiting, "{timeline}");

        let report = timeline.to_string();
        assert!(report.contains("Waiting tasks:"), "{report}");
        assert!(
            report.contains("\"channel-reader\" waiting on channel.receive"),
            "{report}"
        );
        assert!(
            report.contains("\"watch-waiter\" waiting on watch.receive"),
            "{report}"
        );
        assert!(report.contains("15ms"), "{report}");

        assert!(!report.contains("secret-channel-payload"), "{report}");
        assert!(!report.contains("secret-watch-payload"), "{report}");
        assert!(!report.contains("initial-watch-payload"), "{report}");
        let debug_report = format!("{timeline:?}");
        assert!(
            !debug_report.contains("secret-channel-payload"),
            "{debug_report}"
        );
        assert!(
            !debug_report.contains("secret-watch-payload"),
            "{debug_report}"
        );
        assert!(
            !debug_report.contains("initial-watch-payload"),
            "{debug_report}"
        );
    }
}

#[test]
fn failure_displays_the_failing_execution_timeline_and_replay() {
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&executions);
    let failure = testing::check(
        Config {
            iterations: 2,
            ..config()
        },
        move || {
            let index = executions.fetch_add(1, Ordering::Relaxed);
            async move {
                let mut tasks = Tasks::<()>::new();
                tasks.spawn_named(
                    if index == 0 {
                        "passing-worker"
                    } else {
                        "failing-worker"
                    },
                    async move {
                        time::sleep(Duration::from_millis(5)).await;
                        assert_eq!(index, 0, "sensitive-panic-payload");
                        Ok(())
                    },
                );
                tasks.join(Ok(())).await.unwrap();
            }
        },
    )
    .unwrap_err();

    assert_eq!(observed.load(Ordering::Relaxed), 2);
    let report = failure.to_string();
    let replay_line = report.lines().next().unwrap();
    let (_, json) = replay_line.split_once(": ").unwrap();
    assert_eq!(
        serde_json::from_str::<Replay>(json).unwrap(),
        *failure.replay()
    );
    assert_eq!(failure.replay().iteration(), 2);
    assert!(report.contains("Timeline:"), "{report}");
    assert!(
        report.lines().any(|line| {
            line.starts_with("  5ms task#")
                && line.contains(" \"failing-worker\" resumed from time.sleep")
        }),
        "{report}"
    );
    assert!(report.contains("5ms failed (Panic)"), "{report}");
    assert!(!report.contains("passing-worker"), "{report}");
    assert!(!report.contains("sensitive-panic-payload"), "{report}");
}

#[test]
fn explain_preserves_panic_schedule_and_omits_panic_payload() {
    async fn panicking_worker() {
        Tasks::<()>::run(async |tasks| {
            tasks.spawn_named::<(), _>("secret-panicker", async {
                time::sleep(Duration::from_millis(5)).await;
                panic!("sensitive-panic-payload");
            });
            Ok(())
        })
        .await
        .unwrap();
    }

    let failure = testing::check(config(), panicking_worker).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);

    let replayed = testing::replay(failure.replay(), panicking_worker).unwrap_err();
    let timeline = testing::explain(failure.replay(), panicking_worker);

    assert_eq!(timeline.failure(), Some(FailureKind::Panic));
    assert_eq!(timeline.replay(), failure.replay());
    assert_eq!(timeline.replay(), replayed.replay());

    let report = timeline.to_string();
    assert!(report.contains("\"secret-panicker\""), "{report}");
    assert!(report.contains("5ms"), "{report}");
    assert!(report.contains("failed (Panic)"), "{report}");
    assert!(!report.contains("sensitive-panic-payload"), "{report}");
}

#[test]
fn explain_tracks_successive_wakeups_and_caught_panic_waiter_drops() {
    use futures_util::FutureExt;

    async fn two_receivers_and_caught_panic() {
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let mut sleep = std::pin::pin!(time::sleep(Duration::from_millis(50)));
                let waker = futures_util::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                assert!(sleep.as_mut().poll_unpin(&mut cx).is_pending());
                panic!("caught-unwind");
            }))
            .is_err()
        );

        let (tx, rx) = Channel::<u32>::bounded(2).split();
        let mut tasks = Tasks::<()>::new();

        let rx1 = rx.clone();
        tasks.spawn_named("receiver-1", async move {
            assert_eq!(rx1.receive().await, Some(1));
            Ok(())
        });
        time::sleep(Duration::from_millis(1)).await;

        let rx2 = rx.clone();
        tasks.spawn_named("receiver-2", async move {
            assert_eq!(rx2.receive().await, Some(2));
            Ok(())
        });
        time::sleep(Duration::from_millis(1)).await;

        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tasks.join(Ok(())).await.unwrap();
    }

    let record = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        two_receivers_and_caught_panic,
    )
    .unwrap();
    let timeline = testing::explain(&record, two_receivers_and_caught_panic);

    assert_eq!(timeline.failure(), None);
    assert!(timeline.waiting().is_empty(), "{timeline}");

    let channel_wakeups: Vec<Vec<Option<String>>> = timeline
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            TimelineEventKind::Wakeup {
                operation: "channel.receive",
                target_task_names,
                ..
            } => Some(target_task_names.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        channel_wakeups,
        vec![
            vec![Some("receiver-1".to_owned())],
            vec![Some("receiver-2".to_owned())],
        ],
        "{timeline}"
    );
}

#[test]
fn explain_records_wakeup_handoff_when_notified_waiter_is_cancelled() {
    use futures_util::FutureExt;

    async fn handoff_scenario() {
        let (tx, rx) = Channel::<u32>::bounded(1).split();
        let cancel = Cancellation::new();
        let mut tasks = Tasks::<()>::new();

        let rx1 = rx.clone();
        let cancel1 = cancel.clone();
        tasks.spawn_named("cancelled-receiver", async move {
            let mut receive = Box::pin(rx1.receive());
            {
                let waker = futures_util::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                assert!(receive.as_mut().poll_unpin(&mut cx).is_pending());
            }
            cancel1.cancelled().await;
            drop(receive);
            Ok(())
        });
        time::sleep(Duration::from_millis(1)).await;

        let rx2 = rx.clone();
        tasks.spawn_named("surviving-receiver", async move {
            assert_eq!(rx2.receive().await, Some(99));
            Ok(())
        });
        time::sleep(Duration::from_millis(1)).await;

        // Notify cancelled-receiver, then cancel it before it polls `receive` again;
        // dropping its waiter hands the notification off to surviving-receiver.
        tx.send(99).await.unwrap();
        cancel.cancel();
        tasks.join(Ok(())).await.unwrap();
    }

    let record = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        handoff_scenario,
    )
    .unwrap();
    let timeline = testing::explain(&record, handoff_scenario);
    assert_eq!(timeline.failure(), None);

    let channel_wakeups: Vec<Vec<Option<String>>> = timeline
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            TimelineEventKind::Wakeup {
                operation: "channel.receive",
                target_task_names,
                ..
            } => Some(target_task_names.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        channel_wakeups,
        vec![
            vec![Some("cancelled-receiver".to_owned())],
            vec![Some("surviving-receiver".to_owned())],
        ],
        "{timeline}"
    );
}

#[test]
fn shrink_shortens_random_and_pct_replays_and_preserves_corpus_format() {
    async fn scenario() {
        let target = shuttle::future::spawn(async {
            for _ in 0..120 {
                shuttle::future::yield_now().await;
            }
            panic!("original failure");
        });
        let _noise = shuttle::future::spawn(async {
            for _ in 0..200 {
                shuttle::future::yield_now().await;
            }
        });
        target.await.unwrap();
    }

    for mode in [SchedulerMode::Random, SchedulerMode::Pct { depth: 3 }] {
        let original = testing::check(config().with_scheduler(mode), scenario).unwrap_err();
        let shrunk = testing::shrink(original.replay(), scenario).unwrap();
        let step_count = |record: &Replay| {
            serde_json::to_value(record).unwrap()["steps"]
                .as_array()
                .unwrap()
                .len()
        };
        assert!(step_count(original.replay()) >= 150, "{mode:?}");
        assert!(
            step_count(&shrunk) < step_count(original.replay()),
            "{mode:?}"
        );
        assert_eq!(shrunk.scheduler(), mode);
        assert_eq!(shrunk.seed(), original.replay().seed());
        assert_eq!(shrunk.max_steps(), original.replay().max_steps());
        let original_timeline = testing::explain(original.replay(), scenario);
        let path = replay_path("shrunk");
        shrunk.save(&path).unwrap();
        let decoded = Replay::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(decoded, shrunk);
        let timeline = testing::explain(&decoded, scenario);
        assert!(
            timeline.events().len() < original_timeline.events().len(),
            "{mode:?}"
        );
        for _ in 0..2 {
            let failure = testing::replay(&decoded, scenario).unwrap_err();
            assert_eq!(failure.kind(), FailureKind::Panic);
            assert_eq!(failure.replay(), &decoded);
        }
        assert_eq!(testing::shrink(&decoded, scenario), Some(decoded));
    }
}

#[test]
fn shrink_rejects_a_shorter_schedule_with_a_different_panic_in_the_same_task() {
    let observed = Arc::new(AtomicUsize::new(0));
    let observation = Arc::clone(&observed);
    let factory = move || {
        let observation = Arc::clone(&observation);
        async move {
            let noise_started = Arc::new(AtomicUsize::new(0));
            let started = Arc::clone(&noise_started);
            let target = shuttle::future::spawn(async move {
                for _ in 0..20 {
                    shuttle::future::yield_now().await;
                }
                let started = started.load(Ordering::Relaxed);
                observation.fetch_or(started + 1, Ordering::Relaxed);
                if started == 0 {
                    panic!("different failure");
                }
                panic!("original failure");
            });
            let _noise = shuttle::future::spawn(async move {
                noise_started.store(1, Ordering::Relaxed);
                for _ in 0..200 {
                    shuttle::future::yield_now().await;
                }
            });
            target.await.unwrap();
        }
    };

    let original = testing::check(config(), factory.clone()).unwrap_err();
    assert_eq!(observed.load(Ordering::Relaxed), 2);
    let shrunk = testing::shrink(original.replay(), factory.clone()).unwrap();
    assert_ne!(&shrunk, original.replay());
    assert_eq!(observed.load(Ordering::Relaxed), 3);

    observed.store(0, Ordering::Relaxed);
    let failure = testing::replay(&shrunk, factory).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(observed.load(Ordering::Relaxed), 2);
}
