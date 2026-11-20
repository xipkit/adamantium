#![cfg(feature = "tokio")]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "scenario assertions and panic fixtures"
)]

#[cfg(not(target_os = "wasi"))]
mod support;

#[cfg(feature = "deterministic")]
mod virtual_time {
    use adamantium::{
        Actor, Cancellation, ScopeError, SpawnError, Spawner, Tasks,
        testing::{self, Config},
        time,
    };
    use std::{future::pending, time::Duration};

    const TICK: Duration = Duration::from_secs(60 * 60 * 24);
    const GRACE: Duration = Duration::from_secs(60 * 60 * 24 * 10);

    fn check<F, Fut>(factory: F)
    where
        F: Fn() -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()>,
    {
        let config = Config {
            seed: 202,
            iterations: 200,
            max_steps: 20_000,
        };
        let record = testing::check(config, factory.clone()).unwrap();
        assert_eq!(testing::replay(&record, factory).unwrap(), record);
    }

    #[derive(Clone)]
    struct Log {
        start: tokio::time::Instant,
        events: Actor<Vec<(Duration, &'static str)>>,
    }

    impl Log {
        fn new() -> Self {
            Self {
                start: time::now(),
                events: Actor::new(Vec::new()),
            }
        }

        fn note(&self, event: &'static str) {
            let at = time::now() - self.start;
            self.events.with(|events| events.push((at, event)));
        }

        fn on_drop(&self, event: &'static str) -> Noted {
            Noted(Some((self.clone(), event)))
        }

        fn events(&self) -> Vec<(Duration, &'static str)> {
            self.events.snapshot()
        }

        fn elapsed(&self) -> Duration {
            time::now() - self.start
        }
    }

    struct Noted(Option<(Log, &'static str)>);

    impl Noted {
        fn disarm(&mut self) {
            self.0 = None;
        }
    }

    impl Drop for Noted {
        fn drop(&mut self) {
            if let Some((log, event)) = &self.0 {
                log.note(event);
            }
        }
    }

    async fn request(log: Log, duration: Duration) -> Result<(), &'static str> {
        let mut unfinished = log.on_drop("request dropped");
        time::sleep(duration).await;
        unfinished.disarm();
        log.note("request finished");
        Ok(())
    }

    fn cancel_after(tasks: &mut Tasks<&'static str>, signal: &Cancellation, delay: Duration) {
        let signal = signal.clone();
        tasks.spawn(async move {
            time::sleep(delay).await;
            signal.cancel();
            Ok(())
        });
    }

    #[test]
    fn admitted_tasks_finish_after_the_body_stops_accepting() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result = Tasks::<&str>::run_with_grace(signal.clone(), GRACE, async |tasks| {
                let _accepting = log.on_drop("body dropped");
                tasks.spawn(request(log.clone(), TICK * 3));
                cancel_after(tasks, &signal, TICK);
                pending::<Result<(), _>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::Cancelled));
            assert_eq!(
                log.events(),
                [(TICK, "body dropped"), (TICK * 3, "request finished")]
            );
            assert_eq!(log.elapsed(), TICK * 3);
        });
    }

    #[test]
    fn the_grace_period_bounds_the_drain() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result = Tasks::<&str>::run_with_grace(signal.clone(), TICK * 5, async |tasks| {
                tasks.spawn(request(log.clone(), TICK * 20));
                cancel_after(tasks, &signal, TICK);
                pending::<Result<(), _>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::DeadlineExceeded));
            assert_eq!(log.events(), [(TICK * 6, "request dropped")]);
            assert_eq!(log.elapsed(), TICK * 6);
        });
    }

    #[test]
    fn a_zero_grace_period_aborts_at_once() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result =
                Tasks::<&str>::run_with_grace(signal.clone(), Duration::ZERO, async |tasks| {
                    tasks.spawn(request(log.clone(), TICK * 3));
                    cancel_after(tasks, &signal, TICK);
                    pending::<Result<(), _>>().await
                })
                .await;
            assert_eq!(result, Err(ScopeError::DeadlineExceeded));
            assert_eq!(log.events(), [(TICK, "request dropped")]);
        });
    }

    #[test]
    fn an_unrepresentable_grace_period_never_runs_out() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result =
                Tasks::<&str>::run_with_grace(signal.clone(), Duration::MAX, async |tasks| {
                    tasks.spawn(request(log.clone(), TICK * 100));
                    cancel_after(tasks, &signal, TICK);
                    pending::<Result<(), _>>().await
                })
                .await;
            assert_eq!(result, Err(ScopeError::Cancelled));
            assert_eq!(log.events(), [(TICK * 100, "request finished")]);
        });
    }

    #[test]
    fn a_request_before_the_first_poll_never_polls_the_body() {
        check(|| async {
            let signal = Cancellation::new();
            signal.cancel();
            let result = Tasks::<&str>::run_with_grace(signal, GRACE, async |_| {
                panic!("a cancelled body must not start");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
            assert_eq!(result, Err(ScopeError::Cancelled));
        });
    }

    #[test]
    fn a_returned_body_joins_until_shutdown_then_drains() {
        check(|| async {
            let log = Log::new();
            let result = Tasks::<&str>::run_with_grace(Cancellation::new(), TICK, async |tasks| {
                tasks.spawn(request(log.clone(), TICK * 30));
                Ok(42)
            })
            .await;
            assert_eq!(result, Ok(42));
            assert_eq!(log.events(), [(TICK * 30, "request finished")]);

            for (duration, reason, event) in [
                (
                    TICK * 3,
                    ScopeError::Cancelled,
                    (TICK * 3, "request finished"),
                ),
                (
                    TICK * 30,
                    ScopeError::DeadlineExceeded,
                    (TICK * 6, "request dropped"),
                ),
            ] {
                let log = Log::new();
                let signal = Cancellation::new();
                let result =
                    Tasks::<&str>::run_with_grace(signal.clone(), TICK * 5, async |tasks| {
                        tasks.spawn(request(log.clone(), duration));
                        cancel_after(tasks, &signal, TICK);
                        Ok(42)
                    })
                    .await;
                assert_eq!(result, Err(reason));
                assert_eq!(log.events(), [event]);
            }
        });
    }

    #[test]
    fn a_returned_body_error_aborts_without_a_grace_period() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result = Tasks::<&str>::run_with_grace(signal.clone(), GRACE, async |tasks| {
                tasks.spawn(request(log.clone(), TICK));
                Err::<(), _>("body failed")
            })
            .await;
            assert_eq!(result, Err(ScopeError::Failed("body failed")));
            assert_eq!(log.events(), [(Duration::ZERO, "request dropped")]);
            assert!(signal.is_cancelled());
        });
    }

    #[test]
    fn a_task_error_during_the_drain_aborts_the_rest() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result = Tasks::<&str>::run_with_grace(signal.clone(), GRACE, async |tasks| {
                tasks.spawn(request(log.clone(), TICK * 20));
                tasks.spawn(async {
                    time::sleep(TICK * 2).await;
                    Err::<(), _>("request failed")
                });
                cancel_after(tasks, &signal, TICK);
                pending::<Result<(), _>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::Failed("request failed")));
            assert_eq!(log.events(), [(TICK * 2, "request dropped")]);
        });
    }

    #[test]
    fn a_handled_error_during_the_drain_goes_to_its_handler() {
        check(|| async {
            let log = Log::new();
            let signal = Cancellation::new();
            let result = Tasks::<&str>::run_with_grace(signal.clone(), GRACE, async |tasks| {
                tasks.spawn(request(log.clone(), TICK * 4));
                let handler = log.clone();
                tasks.spawn_handled(
                    async {
                        time::sleep(TICK * 2).await;
                        Err::<(), _>("connection reset")
                    },
                    move |_| handler.note("handled"),
                );
                cancel_after(tasks, &signal, TICK);
                pending::<Result<(), _>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::Cancelled));
            assert_eq!(
                log.events(),
                [(TICK * 2, "handled"), (TICK * 4, "request finished")]
            );
        });
    }

    #[test]
    fn an_orphaned_error_while_running_fails_the_scope_at_once() {
        check(|| async {
            let log = Log::new();
            let result = Tasks::<&str>::run_with_grace(Cancellation::new(), GRACE, async |tasks| {
                let _accepting = log.on_drop("body dropped");
                tasks.spawn(request(log.clone(), TICK * 20));
                tasks.spawn(async {
                    time::sleep(TICK).await;
                    Err::<(), _>("request failed")
                });
                pending::<Result<(), _>>().await
            })
            .await;
            assert_eq!(result, Err(ScopeError::Failed("request failed")));
            let mut events = log.events();
            events.sort_unstable();
            assert_eq!(events, [(TICK, "body dropped"), (TICK, "request dropped")]);
        });
    }

    #[test]
    fn spawners_admit_siblings_until_the_drain_ends() {
        check(|| async {
            let kept: Actor<Option<Spawner<&str>>> = Actor::new(None);
            for (grace, reason, expected) in [
                (GRACE, ScopeError::Cancelled, (TICK * 4, "request finished")),
                (
                    TICK * 2,
                    ScopeError::DeadlineExceeded,
                    (TICK * 3, "request dropped"),
                ),
            ] {
                let log = Log::new();
                let signal = Cancellation::new();
                let result = Tasks::<&str>::run_with_grace(signal.clone(), grace, async |tasks| {
                    let spawner = tasks.spawner();
                    kept.with(|kept| *kept = Some(spawner.clone()));
                    let draining = signal.clone();
                    let follow_up = log.clone();
                    tasks.spawn(async move {
                        draining.cancelled().await;
                        time::sleep(TICK).await;
                        spawner
                            .spawn(request(follow_up, TICK * 2))
                            .map_err(|SpawnError| "the drain refused a sibling")?;
                        Ok(())
                    });
                    cancel_after(tasks, &signal, TICK);
                    pending::<Result<(), _>>().await
                })
                .await;
                assert_eq!(result, Err(reason));
                assert_eq!(log.events(), [expected]);
                let late = kept.with(Option::take).unwrap();
                assert_eq!(
                    late.spawn(async { Ok::<_, &str>(()) }).map(drop),
                    Err(SpawnError)
                );
            }
        });
    }
}

#[cfg(not(target_os = "wasi"))]
mod tokio_clock {
    use adamantium::{Cancellation, Reply, ScopeError, Tasks, time};
    use futures_util::FutureExt;
    use std::{
        future::pending,
        panic::AssertUnwindSafe,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    use super::support::timeout;

    const HUNG: Duration = Duration::from_secs(30);
    const GRACE: Duration = Duration::from_secs(10);

    async fn begin<F: Future + Send>(mut future: Pin<&mut F>) {
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_grace_deadline_wins_over_a_drain_ending_at_the_same_poll() {
        let signal = Cancellation::new();
        let request = signal.clone();
        let (finish, gate) = Reply::pair();
        let (finished, done) = Reply::pair();
        let mut scope = Box::pin(Tasks::<&str>::run_with_grace(
            signal,
            GRACE,
            async |tasks| {
                tasks.spawn(async move {
                    gate.receive().await.unwrap();
                    finished.send(()).unwrap();
                    Ok(())
                });
                pending::<Result<(), _>>().await
            },
        ));
        begin(scope.as_mut()).await;
        request.cancel();
        begin(scope.as_mut()).await;
        tokio::time::advance(GRACE).await;
        finish.send(()).unwrap();
        done.receive().await.unwrap();
        assert_eq!(scope.await, Err(ScopeError::DeadlineExceeded));
    }

    #[tokio::test(start_paused = true)]
    async fn started_blocking_work_outlives_the_grace_period() {
        for outcome in [Ok(()), Err("blocking cleanup failed")] {
            let signal = Cancellation::new();
            let (started, running) = Reply::pair();
            #[expect(
                clippy::disallowed_methods,
                reason = "the synchronous test gate needs a bounded blocking wait"
            )]
            let (release, released) = std::sync::mpsc::sync_channel(1);
            let request = signal.clone();
            let mut scope = Box::pin(Tasks::<&str>::run_with_grace(
                signal.clone(),
                GRACE,
                async |tasks| {
                    tasks.spawn_blocking(move || {
                        started.send(()).unwrap();
                        released.recv_timeout(HUNG).unwrap();
                        outcome
                    });
                    pending::<Result<(), _>>().await
                },
            ));
            begin(scope.as_mut()).await;
            // Avoid a virtual-time timeout while waiting for an actual OS thread.
            running.receive().await.unwrap();
            request.cancel();
            begin(scope.as_mut()).await;
            tokio::time::advance(GRACE).await;
            begin(scope.as_mut()).await;
            release.send(()).unwrap();
            let expected = match outcome {
                Ok(()) => ScopeError::DeadlineExceeded,
                Err(error) => ScopeError::Failed(error),
            };
            assert_eq!(scope.await, Err(expected));
        }
    }

    #[test]
    fn queued_blocking_work_starts_during_the_drain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let signal = Cancellation::new();
            let request = signal.clone();
            let (started, running) = Reply::pair();
            #[expect(
                clippy::disallowed_methods,
                reason = "the synchronous test gate needs a bounded blocking wait"
            )]
            let (release, released) = std::sync::mpsc::sync_channel(1);
            let queued_ran = Arc::new(AtomicBool::new(false));
            let ran = queued_ran.clone();
            let mut scope = Box::pin(Tasks::<&str>::run_with_grace(signal, HUNG, async |tasks| {
                tasks.spawn_blocking(move || {
                    started.send(()).unwrap();
                    released.recv_timeout(HUNG).unwrap();
                    Ok(())
                });
                tasks.spawn_blocking(move || {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                });
                pending::<Result<(), _>>().await
            }));
            begin(scope.as_mut()).await;
            timeout(HUNG, running.receive()).await.unwrap().unwrap();
            request.cancel();
            begin(scope.as_mut()).await;
            release.send(()).unwrap();
            assert_eq!(
                timeout(HUNG, scope).await.unwrap(),
                Err(ScopeError::Cancelled)
            );
            assert!(queued_ran.load(Ordering::SeqCst));
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_during_the_drain_is_resumed_after_the_rest_are_aborted() {
        let signal = Cancellation::new();
        let request = signal.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let scope = Tasks::<&str>::run_with_grace(signal, GRACE, async |tasks| {
            tasks.spawn(async move {
                let _guard = guard;
                pending::<()>().await;
                Ok(())
            });
            tasks.spawn::<(), _>(async {
                time::sleep(Duration::from_secs(2)).await;
                panic!("request panicked");
            });
            request.cancel();
            pending::<Result<(), _>>().await
        });
        let start = time::now();
        let panic = AssertUnwindSafe(scope).catch_unwind().await.unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"request panicked"));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(time::now() - start, Duration::from_secs(2));
    }
}
