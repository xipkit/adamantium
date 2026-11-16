#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations and deliberate execution-boundary misuse"
)]

use adamantium::{
    Actor, Cancellation, Channel, Reply, ScopeError, Tasks, Watch, blocking, concurrently,
    concurrently_ordered, concurrently_ordered_until, concurrently_unordered,
    concurrently_unordered_until, first_ok,
    testing::{self, Config, FailureKind},
};
use futures_util::{FutureExt, StreamExt};
use std::{
    future::{Future, pending},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
};

type Scenario = fn() -> Pin<Box<dyn Future<Output = ()>>>;
fn config() -> Config {
    Config {
        seed: 160,
        iterations: 500,
        max_steps: 30_000,
    }
}
fn check(factory: Scenario) {
    let record = testing::check(config(), factory).unwrap();
    assert_eq!(testing::check(config(), factory).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

struct Dropped(Actor<usize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.with(|n| *n += 1);
    }
}
struct PanicDrop;
impl Drop for PanicDrop {
    fn drop(&mut self) {
        panic!("destroyed child");
    }
}

#[test]
fn results_are_claimed_once_and_unclaimed_errors_abort_and_drain() {
    check(|| {
        Box::pin(async {
            let mut tasks = Tasks::<&str>::new();
            let failed = tasks.spawn(async { Err::<(), _>("claimed") });
            assert_eq!(tasks.value(failed).await, Err("claimed"));
            let success = tasks.spawn(async { Ok(42) });
            assert_eq!(tasks.value(success).await, Ok(42));
            assert_eq!(tasks.join(Ok(7)).await, Ok(7));

            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            let mut tasks = Tasks::<&str>::new();
            tasks.spawn(async move {
                let _guard = guard;
                pending::<()>().await;
                Ok(())
            });
            drop(tasks.spawn(async { Err::<(), _>("unclaimed") }));
            assert_eq!(tasks.join(Ok(())).await, Err("unclaimed"));
            assert_eq!(dropped.snapshot(), 1);
        })
    });
}

#[test]
fn poll_panics_are_resumed_by_value_after_sibling_cleanup() {
    check(|| {
        Box::pin(async {
            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(async move {
                let _guard = guard;
                pending::<()>().await;
                Ok(())
            });
            let failed = tasks.spawn(async {
                panic!("poll child");
                #[allow(unreachable_code)]
                Ok(())
            });
            let panic = AssertUnwindSafe(tasks.value(failed))
                .catch_unwind()
                .await
                .unwrap_err();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"poll child"));
            assert_eq!(dropped.snapshot(), 1);
            assert_eq!(tasks.join(Ok(())).await, Ok(()));
        })
    });
}

#[test]
fn abort_time_destruction_panic_wins_over_body_error_and_cancellation() {
    check(|| {
        Box::pin(async {
            for cancel in [false, true] {
                let dropped = Actor::new(0);
                let guard = Dropped(dropped.clone());
                let panic_guard = PanicDrop;
                let mut tasks = Tasks::<&str>::new();
                tasks.spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                    Ok(())
                });
                tasks.spawn(async move {
                    let _guard = panic_guard;
                    pending::<()>().await;
                    Ok(())
                });
                let signal = Cancellation::new();
                if cancel {
                    signal.cancel();
                }
                let panic = AssertUnwindSafe(tasks.join_until(signal, None, Err::<(), _>("body")))
                    .catch_unwind()
                    .await
                    .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"destroyed child"));
                assert_eq!(dropped.snapshot(), 1);
            }
        })
    });
}

#[test]
fn dropping_scope_aborts_children_and_requests_shutdown() {
    check(|| {
        Box::pin(async {
            struct Notify(Option<Reply<()>>);
            impl Drop for Notify {
                fn drop(&mut self) {
                    if let Some(reply) = self.0.take() {
                        reply.send(()).unwrap();
                    }
                }
            }
            let (done, finished) = Reply::pair();
            let guard = Notify(Some(done));
            let signal = Cancellation::new();
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(async move {
                let _guard = guard;
                pending::<()>().await;
                Ok(())
            });
            let scope = tasks.join_until(signal.clone(), None, Ok(()));
            drop(scope); // Never polled; registration and abort must still happen.
            assert!(signal.is_cancelled());
            assert_eq!(finished.receive().await, Some(()));
        })
    });
}

#[test]
fn started_blocking_work_finishes_and_its_error_beats_cancellation() {
    check(|| {
        Box::pin(async {
            let signal = Cancellation::new();
            let worker_signal = signal.clone();
            let finished = Actor::new(false);
            let worker_finished = finished.clone();
            let (started, ready) = Reply::pair();
            let mut tasks = Tasks::<&str>::new();
            tasks.spawn_blocking(move || {
                started.send(()).unwrap();
                while !worker_signal.is_cancelled() {}
                worker_finished.with(|finished| *finished = true);
                Err::<(), _>("blocking failure")
            });
            ready.receive().await.unwrap();
            signal.cancel();
            assert_eq!(
                tasks.join_until(signal, None, Ok(())).await,
                Err(ScopeError::Failed("blocking failure"))
            );
            assert!(finished.snapshot());
            assert_eq!(blocking(|| 42).await, 42);
            let panic = AssertUnwindSafe(blocking(|| panic!("blocking panic")))
                .catch_unwind()
                .await
                .unwrap_err();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"blocking panic"));
        })
    });
}

#[test]
fn queued_blocking_work_can_abort_but_started_work_always_finishes() {
    let seen = Arc::new(Mutex::new([false; 2]));
    let output = seen.clone();
    testing::check(config(), move || {
        let output = output.clone();
        async move {
            let state = Actor::new((false, false));
            let child = state.clone();
            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            let mut tasks = Tasks::<&str>::new();
            let handle = tasks.spawn_blocking(move || {
                let _guard = guard;
                child.with(|state| state.0 = true);
                child.with(|state| state.1 = true);
                Ok(())
            });
            assert_eq!(tasks.join(Err::<(), _>("abort")).await, Err("abort"));
            drop(handle);
            let (started, finished) = state.snapshot();
            assert_eq!(started, finished);
            assert_eq!(dropped.snapshot(), 1);
            output.lock().unwrap()[usize::from(started)] = true;
        }
    })
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), [true, true]);
}

#[test]
fn cancellation_wakes_all_watchers_and_wins_over_ready_data() {
    check(|| {
        Box::pin(async {
            let signal = Cancellation::new();
            let done = Actor::new(0);
            let mut tasks = Tasks::<()>::new();
            for _ in 0..3 {
                let signal = signal.clone();
                let done = done.clone();
                tasks.spawn(async move {
                    signal.cancelled().await;
                    done.with(|n| *n += 1);
                    Ok(())
                });
            }
            signal.cancel();
            signal.cancel();
            tasks.join(Ok(())).await.unwrap();
            assert_eq!(done.snapshot(), 3);
            let (reply, receiver) = Reply::pair();
            reply.send(7).unwrap();
            assert_eq!(
                receiver.receive_until(&signal, None).await,
                Err(adamantium::ReceiveError::Cancelled)
            );
            assert_eq!(receiver.receive().await, Some(7));
            let watch = Watch::new(0);
            let mut watcher = watch.subscribe();
            watch.send(9).unwrap();
            assert_eq!(
                watcher.receive_until(&signal, None).await,
                Err(adamantium::ReceiveError::Cancelled)
            );
            assert_eq!(watcher.receive().await, Some(9));
            let queue = Channel::bounded(1);
            queue.send(3).await.unwrap();
            assert_eq!(
                queue.receive_until(&signal, None).await,
                Err(adamantium::ReceiveError::Cancelled)
            );
            assert_eq!(queue.receive().await, Some(3));
        })
    });
}

#[test]
fn concurrent_maps_and_streams_preserve_results_and_ordering() {
    check(|| {
        Box::pin(async {
            let values = concurrently(0..5, 2, |n| async move { Ok::<_, ()>(n * 2) })
                .await
                .unwrap();
            assert_eq!(values, [0, 2, 4, 6, 8]);
            let ordered = concurrently_ordered(0..5, 2, |n| async move { Ok::<_, ()>(n) });
            assert_eq!(
                ordered.collect::<Vec<_>>().await,
                (0..5).map(Ok).collect::<Vec<_>>()
            );
            let unordered = concurrently_unordered(0..5, 2, |n| async move { Ok::<_, ()>(n) });
            let mut values = unordered
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            values.sort();
            assert_eq!(values, [0, 1, 2, 3, 4]);
            assert_eq!(
                first_ok(
                    0..4,
                    2,
                    |n| async move { if n == 2 { Ok(n) } else { Err(n) } }
                )
                .await,
                Ok(2)
            );
        })
    });
}

#[test]
fn streams_drain_on_cancellation_and_keep_the_parent_observe_only() {
    check(|| {
        Box::pin(async {
            for ordered in [false, true] {
                let signal = Cancellation::new();
                let dropped = Actor::new(0);
                let child_dropped = dropped.clone();
                let f = move |_| {
                    let guard = Dropped(child_dropped.clone());
                    async move {
                        let _guard = guard;
                        pending::<()>().await;
                        Ok::<_, ()>(())
                    }
                };
                let mut stream: Pin<
                    Box<dyn futures_core::Stream<Item = Result<(), ScopeError<()>>> + Send>,
                > = if ordered {
                    Box::pin(concurrently_ordered_until(0..3, 2, signal.clone(), f))
                } else {
                    Box::pin(concurrently_unordered_until(0..3, 2, signal.clone(), f))
                };
                assert!(stream.next().now_or_never().is_none());
                assert!(!signal.is_cancelled());
                signal.cancel();
                assert_eq!(stream.next().await, Some(Err(ScopeError::Cancelled)));
                assert_eq!(dropped.snapshot(), 2);
                assert_eq!(stream.next().await, None);
            }
        })
    });
}

#[test]
fn child_failure_has_seeded_exact_replay() {
    async fn failure() {
        Tasks::<()>::run(async |tasks| {
            tasks.spawn(async {
                panic!("replay child");
                #[allow(unreachable_code)]
                Ok(())
            });
            Ok(())
        })
        .await
        .unwrap();
    }
    let first = testing::check(config(), failure).unwrap_err();
    assert_eq!(first.kind(), FailureKind::Panic);
    assert_eq!(
        testing::check(config(), failure).unwrap_err().replay(),
        first.replay()
    );
    assert_eq!(
        testing::replay(first.replay(), failure)
            .unwrap_err()
            .replay(),
        first.replay()
    );
}

#[test]
fn completed_modeled_handles_reject_later_executions() {
    let escaped = Arc::new(Mutex::new(None));
    let output = escaped.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let output = output.clone();
            async move {
                let mut tasks = Tasks::<()>::new();
                let handle = tasks.spawn(async { Ok(42) });
                tasks.join(Ok(())).await.unwrap();
                *output.lock().unwrap() = Some(handle);
            }
        },
    )
    .unwrap();
    let handle = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(handle))).is_err());
}

#[test]
fn scope_drop_without_an_explicit_drain_cleans_up() {
    check(|| {
        Box::pin(async {
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(pending::<Result<(), ()>>());
            drop(tasks);
        })
    });
}

#[test]
fn modeled_cancellation_and_watch_drop_reject_foreign_execution_without_double_panic() {
    let escaped = Arc::new(Mutex::new(None));
    let output = escaped.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let output = output.clone();
            async move {
                *output.lock().unwrap() = Some((Cancellation::new(), Watch::new(0)));
            }
        },
    )
    .unwrap();
    let (signal, watch) = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(signal))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(watch))).is_err());
}

#[test]
fn task_deadlock_and_step_limit_keep_their_replay_classification() {
    async fn deadlock() {
        let mut tasks = Tasks::<()>::new();
        tasks.spawn(pending::<Result<(), ()>>());
        tasks.join(Ok(())).await.unwrap();
    }
    let failure = testing::check(config(), deadlock).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Deadlock);
    assert_eq!(
        testing::replay(failure.replay(), deadlock)
            .unwrap_err()
            .kind(),
        FailureKind::Deadlock
    );
    async fn spin() {
        let mut tasks = Tasks::<()>::new();
        tasks.spawn(async {
            let count = Actor::new(0);
            loop {
                count.with(|n| *n += 1);
            }
            #[allow(unreachable_code)]
            Ok(())
        });
        tasks.join(Ok(())).await.unwrap();
    }
    let failure = testing::check(
        Config {
            max_steps: 100,
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
}

#[test]
fn stream_abort_panics_beat_cancellation_and_drain_siblings() {
    check(|| {
        Box::pin(async {
            for ordered in [false, true] {
                let signal = Cancellation::new();
                let dropped = Actor::new(0);
                let child_dropped = dropped.clone();
                let f = move |n| {
                    let guard = Dropped(child_dropped.clone());
                    let panic_guard = (n == 0).then(|| PanicDrop);
                    async move {
                        let _guard = guard;
                        let _panic = panic_guard;
                        pending::<()>().await;
                        Ok::<_, ()>(())
                    }
                };
                let mut stream: Pin<
                    Box<dyn futures_core::Stream<Item = Result<(), ScopeError<()>>> + Send>,
                > = if ordered {
                    Box::pin(concurrently_ordered_until(0..3, 2, signal.clone(), f))
                } else {
                    Box::pin(concurrently_unordered_until(0..3, 2, signal.clone(), f))
                };
                assert!(stream.next().now_or_never().is_none());
                signal.cancel();
                let panic = AssertUnwindSafe(stream.next())
                    .catch_unwind()
                    .await
                    .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"destroyed child"));
                assert_eq!(dropped.snapshot(), 2);
            }
        })
    });
}

#[test]
fn foreign_pending_stream_is_rejected_before_yielding_a_buffered_result() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let stream = runtime.block_on(async {
        let (release, gate) = Reply::pair();
        let mut gates = vec![gate].into_iter();
        let mut stream = Box::pin(concurrently_ordered(0..1, 1, move |_| {
            let gate = gates.next().unwrap();
            async move {
                gate.receive().await;
                Ok::<_, ()>(7)
            }
        }));
        assert!(stream.next().now_or_never().is_none());
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        stream
    });
    let stream = Mutex::new(Some(stream));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let mut stream = stream.lock().unwrap().take().unwrap();
            async move {
                stream.next().await;
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
}

#[test]
fn orphaned_errors_stop_the_body_and_claimed_or_kept_ones_do_not() {
    check(|| {
        Box::pin(async {
            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            let result = Tasks::<&str>::run(async move |tasks| {
                tasks.spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                    Ok(())
                });
                // The schedule decides whether the task fails before or after
                // its handle is dropped; either way the error is orphaned.
                drop(tasks.spawn(async { Err::<(), _>("orphaned") }));
                pending::<Result<(), &str>>().await
            })
            .await;
            assert_eq!(result, Err("orphaned"));
            assert_eq!(dropped.snapshot(), 1);

            let result = Tasks::<&str>::run(async |tasks| {
                tasks.spawn_blocking(|| Err::<(), _>("blocking"));
                pending::<Result<(), &str>>().await
            })
            .await;
            assert_eq!(result, Err("blocking"));

            let result = Tasks::<&str>::run(async |tasks| {
                let failed = tasks.spawn(async { Err::<(), _>("handled") });
                assert_eq!(tasks.value(failed).await, Err("handled"));
                let retried = tasks.spawn(async { Ok("recovered") });
                tasks.value(retried).await
            })
            .await;
            assert_eq!(result, Ok("recovered"));

            let finished = Actor::new(false);
            let result = Tasks::<&str>::run(async |tasks| {
                let kept = tasks.spawn(async { Err::<(), _>("kept") });
                for _ in 0..3 {
                    let other = tasks.spawn(async { Ok(()) });
                    tasks.value(other).await?;
                }
                finished.with(|finished| *finished = true);
                drop(kept);
                Ok(())
            })
            .await;
            assert_eq!(result, Err("kept"));
            assert!(finished.snapshot());

            let signal = Cancellation::new();
            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            let result = Tasks::<&str>::run_until(signal.clone(), None, async move |tasks| {
                tasks.spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                    Ok(())
                });
                drop(tasks.spawn(async { Err::<(), _>("orphaned") }));
                pending::<Result<(), &str>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::Failed("orphaned")));
            assert_eq!(dropped.snapshot(), 1);
            assert!(signal.is_cancelled());
        })
    });
}

#[test]
fn run_until_prioritizes_cancellation_and_destroys_scoped_children() {
    check(|| {
        Box::pin(async {
            let signal = Cancellation::new();
            signal.cancel();
            assert_eq!(
                Tasks::<()>::run_until(signal, None, async |_| {
                    panic!("an already cancelled body must not run");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await,
                Err(ScopeError::Cancelled)
            );

            let signal = Cancellation::new();
            let request = signal.clone();
            let dropped = Actor::new(0);
            let guard = Dropped(dropped.clone());
            assert_eq!(
                Tasks::<()>::run_until(signal.clone(), None, async move |tasks| {
                    tasks.spawn(async move {
                        let _guard = guard;
                        pending::<()>().await;
                        Ok(())
                    });
                    request.cancel();
                    pending::<Result<(), ()>>().await
                })
                .await,
                Err(ScopeError::Cancelled)
            );
            assert_eq!(dropped.snapshot(), 1);
            assert!(signal.is_cancelled());
        })
    });
}

#[test]
fn reply_receiver_drop_and_watch_final_close_wake_waiters() {
    check(|| {
        Box::pin(async {
            let (reply, receiver) = Reply::<()>::pair();
            let watch = Watch::new(0);
            let mut watcher = watch.subscribe();
            let mut tasks = Tasks::<()>::new();
            let closed = tasks.spawn(async move {
                reply.closed().await;
                Ok(())
            });
            let changed = tasks.spawn(async move {
                assert_eq!(watcher.receive().await, None);
                Ok(())
            });
            drop(receiver);
            drop(watch);
            tasks.value(closed).await.unwrap();
            tasks.value(changed).await.unwrap();
            tasks.join(Ok(())).await.unwrap();
        })
    });
}

#[test]
fn foreign_scope_rejection_does_not_abort_tasks_with_reused_engine_ids() {
    let escaped = Arc::new(Mutex::new(None));
    let output = escaped.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let output = output.clone();
            async move {
                let mut tasks = Tasks::<()>::new();
                let task = tasks.spawn(async { Ok(()) });
                tasks.value(task).await.unwrap();
                *output.lock().unwrap() = Some(tasks);
            }
        },
    )
    .unwrap();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let foreign = escaped.lock().unwrap().take().unwrap();
            async move {
                let mut tasks = Tasks::<()>::new();
                let (release, gate) = Reply::pair();
                let task = tasks.spawn(async move {
                    gate.receive().await;
                    Ok(42)
                });
                assert!(catch_unwind(AssertUnwindSafe(|| drop(foreign))).is_err());
                release.send(()).unwrap();
                assert_eq!(tasks.value(task).await, Ok(42));
                tasks.join(Ok(())).await.unwrap();
            }
        },
    )
    .unwrap();
}
