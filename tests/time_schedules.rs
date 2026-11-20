#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "test observations and deliberate foreign-execution misuse"
)]

use adamantium::{
    Actor, Cancellation, Channel, ReceiveError, ScopeError, SendError, Tasks,
    concurrently_ordered_until, concurrently_unordered_until,
    testing::{self, Config, FailureKind},
    time,
};
use futures_util::{FutureExt, StreamExt};
use std::{
    future::{Future, pending, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::{Pin, pin},
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

const TICK: Duration = Duration::from_secs(60 * 60 * 24);

fn config() -> Config {
    Config {
        seed: 161,
        iterations: 500,
        max_steps: 10_000,
    }
}

fn check<F, Fut>(factory: F)
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

#[test]
fn each_iteration_and_replay_restart_at_the_same_origin() {
    let origin = Arc::new(Mutex::new(None));
    let factory = move || {
        let origin = origin.clone();
        async move {
            let start = time::now();
            assert_eq!(*origin.lock().unwrap().get_or_insert(start), start);
            time::sleep(TICK).await;
            assert_eq!(time::now(), start + TICK);
        }
    };
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

#[test]
fn virtual_sleep_and_absolute_deadlines_run_without_a_tokio_runtime() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let later = time::sleep(TICK);
            time::sleep_until(start + TICK).await;
            assert_eq!(time::now(), start + TICK);
            // Relative sleeps start at first poll, just like the production wrapper.
            later.await;
            assert_eq!(time::now(), start + TICK * 2);
            for deadline in [start, time::now(), time::now() - Duration::from_nanos(1)] {
                assert_eq!(time::sleep_until(deadline).now_or_never(), Some(()));
            }
            assert_eq!(time::sleep(Duration::ZERO).now_or_never(), Some(()));
            time::sleep(Duration::from_nanos(1)).await;
            assert_eq!(time::now(), start + TICK * 2 + Duration::from_nanos(1));
        })
    });
}

#[test]
fn oversized_sleeps_and_retry_backoffs_finish_and_replay() {
    check(|| async {
        let start = time::now();
        time::sleep(Duration::MAX).await;
        let delay = time::now() - start;
        assert!(delay > TICK);
        let mut calls = 0;
        let result = adamantium::retry(
            &Cancellation::new(),
            None,
            adamantium::Backoff::exponential(Duration::MAX, Duration::MAX).attempts(3),
            || {
                calls += 1;
                async { Err::<(), _>("no") }
            },
        )
        .await;
        assert_eq!(result, Err(ScopeError::Failed("no")));
        assert_eq!(calls, 3);
        assert_eq!(time::now(), start + delay * 3);
    });
}

#[cfg(not(target_os = "wasi"))]
#[test]
fn modeled_relative_sleep_delays_match_tokio() {
    // Whole milliseconds: Tokio's timer wheel rounds shorter waits up.
    const DURATIONS: [Duration; 5] = [
        Duration::ZERO,
        Duration::from_millis(1),
        TICK,
        TICK.checked_mul(365 * 31).unwrap(),
        Duration::MAX,
    ];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let expected = runtime.block_on(async {
        let mut expected = Vec::new();
        for duration in DURATIONS {
            let start = time::now();
            time::sleep(duration).await;
            expected.push((duration, time::now() - start));
        }
        expected
    });
    drop(runtime);
    check(move || {
        let expected = expected.clone();
        async move {
            for (duration, delay) in expected {
                let start = time::now();
                time::sleep(duration).await;
                assert_eq!(time::now() - start, delay, "sleep({duration:?})");
            }
        }
    });
}

#[test]
fn oversized_sleep_fallback_cannot_overflow_near_the_instant_limit() {
    check(|| async {
        let mut limit = time::now();
        let mut step = Duration::MAX;
        while !step.is_zero() {
            if let Some(later) = limit.checked_add(step) {
                limit = later;
            }
            step /= 2;
        }
        assert!(limit.checked_add(TICK).is_none());
        time::sleep_until(limit).await;
        time::sleep(Duration::MAX).await;
        assert!(time::now() >= limit);
    });
}

#[test]
fn oversized_sleeps_and_backoffs_allow_earlier_shutdown() {
    check(|| async {
        let start = time::now();
        assert_eq!(
            Tasks::<()>::run_until(Cancellation::new(), Some(start + TICK), async |_| {
                time::sleep(Duration::MAX).await;
                Ok(())
            },)
            .await,
            Err(ScopeError::DeadlineExceeded)
        );
        assert_eq!(time::now(), start + TICK);

        for cancelled in [false, true] {
            let start = time::now();
            let signal = Cancellation::new();
            let mut tasks = Tasks::<()>::new();
            let deadline = if cancelled {
                let request = signal.clone();
                tasks.spawn(async move {
                    time::sleep(TICK).await;
                    request.cancel();
                    Ok(())
                });
                None
            } else {
                Some(start + TICK)
            };
            let mut calls = 0;
            let result = adamantium::retry(
                &signal,
                deadline,
                adamantium::Backoff::constant(Duration::MAX),
                || {
                    calls += 1;
                    async { Err::<(), _>("no") }
                },
            )
            .await;
            assert_eq!(
                result,
                Err(if cancelled {
                    ScopeError::Cancelled
                } else {
                    ScopeError::DeadlineExceeded
                })
            );
            assert_eq!(calls, 1);
            assert_eq!(time::now(), start + TICK);
            tasks.join(Ok(())).await.unwrap();
        }
    });
}

#[test]
fn time_stays_frozen_while_any_task_is_runnable_or_self_woken() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let timer = time::sleep(TICK);
            let mut timer = pin!(timer);
            assert!(timer.as_mut().now_or_never().is_none());
            let mut polls = 0;
            poll_fn(|cx| {
                assert_eq!(time::now(), start);
                polls += 1;
                if polls == 10 {
                    return Poll::Ready(());
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            })
            .await;
            let mut tasks = Tasks::<()>::new();
            for _ in 0..3 {
                tasks.spawn(async move {
                    let actor = Actor::new(0);
                    for _ in 0..10 {
                        actor.with(|n| *n += 1);
                        assert_eq!(time::now(), start);
                    }
                    Ok(())
                });
            }
            tasks.join(Ok(())).await.unwrap();
            assert_eq!(time::now(), start);
            timer.await;
            assert_eq!(time::now(), start + TICK);
        })
    });
}

#[test]
fn earliest_deadlines_wake_together_before_later_deadlines() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let seen = Actor::new(Vec::new());
            let mut tasks = Tasks::<()>::new();
            for n in [3, 1, 2, 1] {
                let seen = seen.clone();
                tasks.spawn(async move {
                    time::sleep_until(start + TICK * n).await;
                    assert_eq!(time::now(), start + TICK * n);
                    seen.with(|values| values.push(n));
                    Ok(())
                });
            }
            tasks.join(Ok(())).await.unwrap();
            assert_eq!(seen.snapshot(), [1, 1, 2, 3]);
        })
    });
}

#[test]
fn equal_deadline_resume_order_is_explored() {
    let seen = Arc::new(Mutex::new([false; 2]));
    let output = seen.clone();
    check(move || {
        let output = output.clone();
        async move {
            let deadline = time::now() + TICK;
            let first = Actor::new(None);
            let mut tasks = Tasks::<()>::new();
            for n in 0..2 {
                let first = first.clone();
                tasks.spawn(async move {
                    time::sleep_until(deadline).await;
                    first.with(|first| {
                        first.get_or_insert(n);
                    });
                    Ok(())
                });
            }
            tasks.join(Ok(())).await.unwrap();
            output.lock().unwrap()[first.snapshot().unwrap()] = true;
        }
    });
    assert_eq!(*seen.lock().unwrap(), [true, true]);
}

#[test]
fn dropping_the_earliest_sleep_unregisters_it_and_repoll_updates_the_waker() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let mut cancelled = Box::pin(time::sleep(TICK));
            let mut retained = Box::pin(time::sleep(TICK * 3));
            assert!(futures_util::poll!(&mut cancelled).is_pending());
            assert!(retained.as_mut().now_or_never().is_none());
            drop(cancelled);
            let seen = Actor::new(None);
            let child = seen.clone();
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(async move {
                // Moving between logical tasks in one execution is supported.
                retained.await;
                child.with(|seen| *seen = Some(time::now()));
                Ok(())
            });
            let mut next = pin!(time::sleep(TICK * 2));
            poll_fn(|cx| {
                let now = time::now();
                assert!(
                    now == start || now == start + TICK * 2,
                    "cancelled timer woke its task"
                );
                next.as_mut().poll(cx)
            })
            .await;
            assert_eq!(time::now(), start + TICK * 2);
            assert_eq!(seen.snapshot(), None);
            tasks.join(Ok(())).await.unwrap();
            assert_eq!(seen.snapshot(), Some(start + TICK * 3));
        })
    });
}

#[test]
fn channel_deadlines_preserve_ready_values_unsent_values_and_cancellation_priority() {
    check(|| {
        Box::pin(async {
            let queue = Channel::bounded(1);
            let signal = Cancellation::new();
            let deadline = time::now() + TICK;
            assert_eq!(
                queue.receive_until(&signal, Some(deadline)).await,
                Err(ReceiveError::DeadlineExceeded)
            );
            assert_eq!(time::now(), deadline);
            queue.send(7).await.unwrap();
            assert_eq!(
                queue.receive_until(&signal, Some(deadline)).await,
                Err(ReceiveError::DeadlineExceeded)
            );
            assert_eq!(
                queue.send_until(&signal, Some(deadline), 8).await,
                Err(SendError::DeadlineExceeded(8))
            );
            signal.cancel();
            assert_eq!(
                queue.receive_until(&signal, Some(deadline)).await,
                Err(ReceiveError::Cancelled)
            );
            assert_eq!(
                queue.send_until(&signal, Some(deadline), 9).await,
                Err(SendError::Cancelled(9))
            );
            assert_eq!(queue.receive().await, Some(7));
            assert_eq!(queue.try_receive(), Err(adamantium::TryReceiveError::Empty));
        })
    });
}

#[test]
fn readiness_before_the_deadline_cancels_the_timeout_without_advancing_time() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let queue = Channel::bounded(1);
            let sender = queue.clone();
            let signal = Cancellation::new();
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(async move {
                time::sleep(TICK).await;
                sender.send(42).await.unwrap();
                Ok(())
            });
            assert_eq!(
                queue.receive_until(&signal, Some(start + TICK * 3)).await,
                Ok(42)
            );
            assert_eq!(time::now(), start + TICK);
            tasks.join(Ok(())).await.unwrap();
            time::sleep(TICK).await;
            assert_eq!(time::now(), start + TICK * 2);

            queue.send(1).await.unwrap();
            let deadline = time::now() + TICK;
            assert_eq!(
                queue.send_until(&signal, Some(deadline), 2).await,
                Err(SendError::DeadlineExceeded(2))
            );
            assert_eq!(queue.receive().await, Some(1));
            assert_eq!(time::now(), deadline);
        })
    });
}

#[test]
fn retry_and_rate_limit_waits_use_the_same_virtual_clock() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let signal = Cancellation::new();
            let mut calls = 0;
            let result = adamantium::retry(
                &signal,
                Some(start + TICK * 3),
                adamantium::Backoff::constant(TICK),
                || {
                    calls += 1;
                    async { Err::<(), _>("retry") }
                },
            )
            .await;
            assert_eq!(result, Err(ScopeError::DeadlineExceeded));
            assert_eq!(calls, 3);
            assert_eq!(time::now(), start + TICK * 3);
            let limit = adamantium::RateLimit::new(1, TICK);
            for _ in 0..2 {
                assert_eq!(
                    limit
                        .run_until(&signal, None, || async { Ok::<_, ()>(42) })
                        .await,
                    Ok(42)
                );
            }
            assert_eq!(time::now(), start + TICK * 4);
        })
    });
}

#[test]
fn scope_body_deadline_cancels_its_timer_and_drains_sleeping_children() {
    check(|| {
        Box::pin(async {
            let start = time::now();
            let signal = Cancellation::new();
            let dropped = Actor::new(0);
            let body_guard = Dropped(dropped.clone());
            let child_guard = Dropped(dropped.clone());
            assert_eq!(
                Tasks::<()>::run_until(signal.clone(), Some(start + TICK), async move |tasks| {
                    let _guard = body_guard;
                    tasks.spawn(async move {
                        let _guard = child_guard;
                        time::sleep(TICK * 10).await;
                        Ok(())
                    });
                    time::sleep(TICK * 20).await;
                    Ok(())
                })
                .await,
                Err(ScopeError::DeadlineExceeded)
            );
            assert!(signal.is_cancelled());
            assert_eq!(dropped.snapshot(), 2);
            assert_eq!(time::now(), start + TICK);
        })
    });
}

#[test]
fn channel_readiness_and_cancellation_compete_at_the_same_deadline() {
    let seen = Arc::new(Mutex::new([false; 2]));
    let output = seen.clone();
    check(move || {
        let output = output.clone();
        async move {
            let deadline = time::now() + TICK;
            let queue = Channel::bounded(1);
            let sender = queue.clone();
            let signal = Cancellation::new();
            let cancel = signal.clone();
            let mut tasks = Tasks::<()>::new();
            tasks.spawn(async move {
                time::sleep_until(deadline).await;
                cancel.cancel();
                sender.send(42).await.unwrap();
                Ok(())
            });
            let result = queue.receive_until(&signal, Some(deadline)).await;
            let cancelled = match result {
                Err(ReceiveError::Cancelled) => true,
                Err(ReceiveError::DeadlineExceeded) => false,
                result => panic!("deadline must beat ready data: {result:?}"),
            };
            output.lock().unwrap()[usize::from(cancelled)] = true;
            tasks.join(Ok(())).await.unwrap();
            assert_eq!(queue.receive().await, Some(42));
        }
    });
    assert_eq!(*seen.lock().unwrap(), [true, true]);
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
        panic!("deadline cleanup panic");
    }
}

#[test]
fn task_deadlines_abort_sleepers_drain_and_keep_cleanup_panic_priority() {
    check(|| {
        Box::pin(async {
            for panic in [false, true] {
                let start = time::now();
                let signal = Cancellation::new();
                let dropped = Actor::new(0);
                let guard = Dropped(dropped.clone());
                let panic_guard = panic.then(|| PanicDrop);
                let mut tasks = Tasks::<()>::new();
                tasks.spawn(async move {
                    let _guard = guard;
                    let _panic_guard = panic_guard;
                    time::sleep(TICK * 10).await;
                    Ok(())
                });
                let result =
                    AssertUnwindSafe(tasks.join_until(signal.clone(), Some(start + TICK), Ok(())))
                        .catch_unwind()
                        .await;
                if panic {
                    assert_eq!(
                        result.unwrap_err().downcast_ref::<&str>(),
                        Some(&"deadline cleanup panic")
                    );
                } else {
                    assert_eq!(result.unwrap(), Err(ScopeError::DeadlineExceeded));
                }
                assert!(signal.is_cancelled());
                assert_eq!(dropped.snapshot(), 1);
                assert_eq!(time::now(), start + TICK);
            }
        })
    });
}

#[test]
fn a_child_error_at_the_deadline_wins_if_recorded_before_abort() {
    let seen = Arc::new(Mutex::new([false; 2]));
    let output = seen.clone();
    check(move || {
        let output = output.clone();
        async move {
            let deadline = time::now() + TICK;
            let mut tasks = Tasks::<&str>::new();
            drop(tasks.spawn(async move {
                time::sleep_until(deadline).await;
                // Without a scheduling point between waking and failing, the
                // abort comes first in too few schedules to rely on reaching.
                tokio::task::yield_now().await;
                Err::<(), _>("timed failure")
            }));
            let result = tasks
                .join_until(Cancellation::new(), Some(deadline), Ok(()))
                .await;
            let failed = match result {
                Err(ScopeError::Failed("timed failure")) => true,
                Err(ScopeError::DeadlineExceeded) => false,
                result => panic!("unexpected deadline outcome: {result:?}"),
            };
            output.lock().unwrap()[usize::from(failed)] = true;
            assert_eq!(time::now(), deadline);
        }
    });
    assert_eq!(*seen.lock().unwrap(), [true, true]);
}

#[test]
fn ready_task_body_loses_to_expiry_and_cancellation_wins_the_tie() {
    check(|| {
        Box::pin(async {
            for cancel in [false, true] {
                let signal = Cancellation::new();
                if cancel {
                    signal.cancel();
                }
                let result = Tasks::<()>::run_until(signal, Some(time::now()), async |_| {
                    panic!("expired body must not run");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await;
                assert_eq!(
                    result,
                    Err(if cancel {
                        ScopeError::Cancelled
                    } else {
                        ScopeError::DeadlineExceeded
                    })
                );
            }
        })
    });
}

#[test]
fn streams_with_virtual_work_keep_order_and_cancel_and_drain() {
    check(|| {
        Box::pin(async {
            for ordered in [false, true] {
                let signal = Cancellation::new();
                let request = signal.clone();
                let start = time::now();
                let mut tasks = Tasks::<()>::new();
                tasks.spawn(async move {
                    time::sleep_until(start + TICK * 3).await;
                    request.cancel();
                    Ok(())
                });
                let dropped = Actor::new(0);
                let child = dropped.clone();
                let f = move |n| {
                    let guard = Dropped(child.clone());
                    async move {
                        let _guard = guard;
                        time::sleep_until(start + TICK * (n + 1)).await;
                        Ok::<_, ()>(n)
                    }
                };
                let stream: Pin<
                    Box<dyn futures_core::Stream<Item = Result<u32, ScopeError<()>>> + Send>,
                > = if ordered {
                    Box::pin(concurrently_ordered_until(0..5, 5, signal.clone(), f))
                } else {
                    Box::pin(concurrently_unordered_until(0..5, 5, signal.clone(), f))
                };
                let values = stream.collect::<Vec<_>>().await;
                assert_eq!(values.first(), Some(&Ok(0)));
                assert_eq!(values.get(1), Some(&Ok(1)));
                assert_eq!(values.last(), Some(&Err(ScopeError::Cancelled)));
                assert!(values.len() == 3 || values.len() == 4);
                if values.len() == 4 {
                    assert_eq!(values.get(2), Some(&Ok(2)));
                }
                assert_eq!(dropped.snapshot(), 5);
                assert_eq!(time::now(), start + TICK * 3);
                tasks.join(Ok(())).await.unwrap();
            }
        })
    });
}

#[test]
fn a_timer_does_not_hide_deadlock_or_runnable_step_exhaustion() {
    async fn deadlock() {
        let mut cancelled = Box::pin(time::sleep(TICK));
        assert!(futures_util::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        pending::<()>().await;
    }
    async fn spin() {
        let start = time::now();
        let mut sleep = pin!(time::sleep(TICK));
        assert!(sleep.as_mut().now_or_never().is_none());
        let actor = Actor::new(0);
        loop {
            assert_eq!(time::now(), start);
            actor.with(|n| *n += 1);
        }
    }
    let failed = testing::check(config(), deadlock).unwrap_err();
    assert_eq!(failed.kind(), FailureKind::Deadlock);
    assert_eq!(
        testing::replay(failed.replay(), deadlock)
            .unwrap_err()
            .replay(),
        failed.replay()
    );
    let failed = testing::check(
        Config {
            max_steps: 100,
            ..config()
        },
        spin,
    )
    .unwrap_err();
    assert_eq!(failed.kind(), FailureKind::StepLimit);
    assert_eq!(
        testing::replay(failed.replay(), spin).unwrap_err().replay(),
        failed.replay()
    );
}

#[test]
fn failure_after_virtual_time_has_identical_seed_and_schedule_replay() {
    async fn failure() {
        let start = time::now();
        time::sleep(TICK).await;
        assert_eq!(time::now(), start + TICK);
        panic!("after a virtual day");
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

type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

fn escaped_sleep() -> Sleep {
    let held = Arc::new(Mutex::new(None));
    let output = held.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let output = output.clone();
            async move {
                let mut sleep: Sleep = Box::pin(time::sleep(TICK));
                assert!(sleep.as_mut().now_or_never().is_none());
                *output.lock().unwrap() = Some(sleep);
            }
        },
    )
    .unwrap();
    held.lock().unwrap().take().unwrap()
}

#[test]
fn foreign_sleep_poll_and_drop_are_rejected_without_touching_reused_timer_ids() {
    assert!(catch_unwind(AssertUnwindSafe(|| drop(escaped_sleep()))).is_err());
    let foreign = Mutex::new(Some((escaped_sleep(), escaped_sleep())));
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let (mut foreign, foreign_drop) = foreign.lock().unwrap().take().unwrap();
            async move {
                let start = time::now();
                let mut local = pin!(time::sleep(TICK * 2));
                assert!(local.as_mut().now_or_never().is_none());
                assert!(
                    catch_unwind(AssertUnwindSafe(|| foreign.as_mut().now_or_never())).is_err()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(foreign_drop))).is_err());
                drop(foreign);
                local.await;
                assert_eq!(time::now(), start + TICK * 2);
            }
        },
    )
    .unwrap();
}

#[test]
fn a_prepolled_production_sleep_is_rejected_before_polling_tokio_in_the_model() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let sleep = {
        let _entered = runtime.enter();
        let mut sleep = Box::pin(time::sleep(TICK));
        assert!(sleep.as_mut().now_or_never().is_none());
        sleep
    };
    let held = Mutex::new(Some(sleep));
    let failed = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || held.lock().unwrap().take().unwrap(),
    )
    .unwrap_err();
    assert_eq!(failed.kind(), FailureKind::Panic);
}

#[test]
fn virtual_interval_missed_tick_policies_and_zero_period_replay() {
    check(|| async {
        let start = time::now();
        let period = TICK;

        // Delay: misses two ticks and schedules the next a full period after now.
        let mut delay = time::Interval::new(period);
        assert_eq!(delay.tick().await, start);
        time::sleep(period * 2 + Duration::from_millis(5)).await;
        let missed = delay.tick().await;
        assert_eq!(missed, start + period * 2 + Duration::from_millis(5));
        assert_eq!(delay.tick().await, missed + period);

        // Skip: misses ticks and snaps back to the original phase grid.
        let origin = time::now();
        let mut skip =
            time::Interval::new(period).with_missed_tick_behavior(time::MissedTickBehavior::Skip);
        assert_eq!(skip.tick().await, origin);
        time::sleep(period * 2 + Duration::from_millis(5)).await;
        assert_eq!(
            skip.tick().await,
            origin + period * 2 + Duration::from_millis(5)
        );
        assert_eq!(skip.tick().await, origin + period * 3);

        // Zero period never panics under either policy.
        for behavior in [
            time::MissedTickBehavior::Delay,
            time::MissedTickBehavior::Skip,
        ] {
            let now = time::now();
            let mut zero = time::Interval::new(Duration::ZERO).with_missed_tick_behavior(behavior);
            assert_eq!(zero.tick().await, now);
            assert_eq!(zero.tick().await, now);
        }
    });
}

#[test]
fn virtual_interval_tick_until_cancels_cleanly_and_loses_tie_to_cancellation() {
    check(|| async {
        let start = time::now();
        let cancellation = Cancellation::new();
        let mut interval = time::Interval::at(start + TICK, TICK);

        // Pre-cancelled wins over an immediately ready tick.
        let pre = Cancellation::new();
        pre.cancel();
        let mut ready = time::Interval::new(TICK);
        assert_eq!(ready.tick_until(&pre).await, None);

        // Concurrent task cancels before the first tick fires.
        Tasks::run(async |t| {
            let cancel_handle = cancellation.clone();
            t.spawn(async move {
                time::sleep(Duration::from_secs(60)).await;
                cancel_handle.cancel();
                Ok::<(), ()>(())
            });
            assert_eq!(interval.tick_until(&cancellation).await, None);
            assert_eq!(time::now(), start + Duration::from_secs(60));
            Ok(())
        })
        .await
        .unwrap();

        // Cancellation wins when a tick and cancel become ready at the same instant.
        let tie_cancel = Cancellation::new();
        let mut tie = time::Interval::at(time::now() + TICK, TICK);
        Tasks::run(async |t| {
            let cancel_handle = tie_cancel.clone();
            t.spawn(async move {
                time::sleep(TICK).await;
                cancel_handle.cancel();
                Ok::<(), ()>(())
            });
            let _ = tie.tick_until(&tie_cancel).await;
            Ok(())
        })
        .await
        .unwrap();

        // Zero-period tick_until yields so a sibling task can cancel on the same scheduler.
        let zero_cancel = Cancellation::new();
        let worker_cancel = zero_cancel.clone();
        Tasks::run(async move |t| {
            t.spawn(async move {
                let mut zero = time::Interval::new(Duration::ZERO);
                while zero.tick_until(&worker_cancel).await.is_some() {}
                Ok::<(), ()>(())
            });
            let mut yielder = time::Interval::new(Duration::ZERO);
            yielder.tick().await;
            zero_cancel.cancel();
            Ok(())
        })
        .await
        .unwrap();
    });
}
