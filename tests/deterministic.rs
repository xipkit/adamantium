#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations and deliberate foreign-runtime misuse"
)]

use adamantium::{
    Actor, Cancellation, Channel, Reply, Tasks, TryReceiveError, Watch,
    testing::{self, Config, FailureKind, Replay},
};
use std::{
    future::pending,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};

fn config() -> Config {
    Config {
        seed: 143,
        iterations: 100,
        max_steps: 2_000,
    }
}

async fn broken_increment() {
    let count = Actor::new(0);
    let workers = (0..2)
        .map(|_| {
            let count = count.clone();
            async move {
                // No awaits between the read and write: switching only on Pending
                // would never detect this lost update.
                let previous = count.read(|n| *n);
                count.with(|n| *n = previous + 1);
            }
        })
        .collect::<Vec<_>>();
    testing::parallel(workers).await;
    assert_eq!(count.snapshot(), 2, "lost update");
}

#[test]
fn finds_and_replays_a_race_without_awaits() {
    let failure = testing::check(config(), broken_increment).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert!(failure.replay().iteration() <= 100);
    let again = testing::replay(failure.replay(), broken_increment).unwrap_err();
    assert_eq!(again.kind(), FailureKind::Panic);
    assert_eq!(failure.replay(), again.replay());
    let same_seed = testing::check(config(), broken_increment).unwrap_err();
    assert_eq!(failure.replay(), same_seed.replay());
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
    let failure = testing::check(
        Config {
            max_steps: 20,
            ..config()
        },
        spin,
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::StepLimit);
    assert_eq!(
        testing::replay(failure.replay(), spin).unwrap_err().kind(),
        FailureKind::StepLimit
    );
    let failure = testing::check(config(), pending::<()>).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Deadlock);
    assert_eq!(
        testing::replay(failure.replay(), pending::<()>)
            .unwrap_err()
            .kind(),
        FailureKind::Deadlock
    );
    // An engine failure must reset the execution mark for subsequent work.
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
fn unsupported_primitives_cannot_silently_use_production_backends() {
    assert!(
        testing::check(config(), || async {
            let _channel = Channel::<u8>::bounded(1);
        })
        .is_err()
    );
    assert!(
        testing::check(config(), || async {
            let _tasks = Tasks::<()>::new();
        })
        .is_err()
    );
    assert!(
        testing::check(config(), || async {
            let _cancel = Cancellation::new();
        })
        .is_err()
    );
    assert!(
        testing::check(config(), || async {
            adamantium::blocking(|| ()).await;
        })
        .is_err()
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
    testing::check(config(), || async {}).unwrap();
}

#[test]
fn trace_files_round_trip_without_payloads_and_validate_versions() {
    let failure = testing::check(config(), || async {
        panic!("private fixture payload");
    })
    .unwrap_err();
    let path = std::env::temp_dir().join(format!("adamantium-replay-{}.json", std::process::id()));
    failure.replay().save(&path).unwrap();
    let json = std::fs::read_to_string(&path).unwrap();
    assert!(!json.contains("private fixture payload"));
    assert_eq!(Replay::load(&path).unwrap(), *failure.replay());
    let incompatible = json.replace("shuttle=0.9.10", "shuttle=0.0.0");
    std::fs::write(&path, incompatible).unwrap();
    assert!(Replay::load(&path).is_err());
    std::fs::remove_file(path).unwrap();
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
fn replies_and_watches_reject_the_harness() {
    /// Runs `body` once in the harness with a value made outside it, and
    /// returns how the execution failed.
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

    assert!(
        testing::check(config(), || async {
            let _reply = Reply::<u8>::pair();
        })
        .is_err()
    );
    assert!(
        testing::check(config(), || async {
            let _watch = Watch::new(0);
        })
        .is_err()
    );

    let (reply, answer) = Reply::<String>::pair();
    assert_eq!(
        carried_in(reply, |reply| drop(reply.send(String::from("sent")))),
        FailureKind::Panic
    );
    assert_eq!(
        answer.try_receive(),
        Err(TryReceiveError::Closed),
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
    // Unwinding drops the carried watch without panicking a second time.
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

    // A destructor that sends while unwinding reports the first panic
    // rather than aborting on a second.
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
