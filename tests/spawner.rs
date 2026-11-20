#![cfg(feature = "tokio")]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "scenario assertions and panic fixtures are shared by Tokio tests and the deterministic harness"
)]

mod support;

use adamantium::{Actor, Cancellation, Reply, ScopeError, Spawner, Tasks, time};
#[cfg(not(target_os = "wasi"))]
use futures_util::FutureExt;
#[cfg(any(not(target_os = "wasi"), feature = "deterministic"))]
use std::panic::AssertUnwindSafe;
use std::{future::pending, pin::Pin, task::Poll, time::Duration};

async fn begin<F: Future + Send>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

macro_rules! scenario {
    ($name:ident, $body:block) => {
        mod $name {
            use super::*;

            async fn run() $body

            #[tokio::test]
            async fn current_thread() {
                support::timeout(Duration::from_secs(30), run()).await.unwrap();
            }

            #[cfg(not(target_os = "wasi"))]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn multi_thread() {
                support::timeout(Duration::from_secs(30), run()).await.unwrap();
            }

            #[cfg(feature = "deterministic")]
            #[test]
            fn schedules_and_replay() {
                let replay = adamantium::testing::check(
                    adamantium::testing::Config {
                        seed: 192,
                        iterations: 200,
                        max_steps: 30_000,
                    },
                    run,
                ).unwrap();
                assert_eq!(adamantium::testing::replay(&replay, run).unwrap(), replay);
            }
        }
    };
}

scenario!(spawn_variants_share_scope_results_and_admission, {
    let mut scope = Tasks::<&str>::new();
    let spawner = scope.spawner();
    let handled = Actor::new(0);
    let first = handled.clone();
    let second = handled.clone();
    spawner
        .spawn_handled(async { Err(2) }, move |error| {
            first.with(|sum| *sum += error)
        })
        .unwrap();
    spawner
        .spawn_handled_named("handled async", async { Err(3) }, move |error| {
            second.with(|sum| *sum += error);
        })
        .unwrap();

    let named = spawner
        .spawn_named("named async", async { Err::<(), _>("claimed") })
        .unwrap();
    let blocking = spawner.spawn_blocking(|| Ok(7)).unwrap();
    let named_blocking = spawner
        .spawn_blocking_named("named blocking", || Ok(42))
        .unwrap();
    assert_eq!(scope.value(named).await, Err("claimed"));
    assert_eq!(scope.value(blocking).await, Ok(7));
    assert_eq!(scope.value(named_blocking).await, Ok(42));
    assert_eq!(scope.join(Ok(())).await, Ok(()));
    assert_eq!(handled.snapshot(), 5);

    assert!(
        spawner
            .spawn_named::<(), _>("closed", async { panic!("polled after closure") })
            .is_err()
    );
    assert!(
        spawner
            .spawn_handled(async { Err(()) }, |()| panic!("ran after closure"))
            .is_err()
    );
    assert!(
        spawner
            .spawn_handled_named("closed", async { Err(()) }, |()| panic!(
                "ran after closure"
            ))
            .is_err()
    );
    assert!(
        spawner
            .spawn_blocking::<(), _>(|| panic!("started after closure"))
            .is_err()
    );
    assert!(
        spawner
            .spawn_blocking_named::<(), _>("closed", || panic!("started after closure"))
            .is_err()
    );
});

#[cfg(feature = "deterministic")]
#[test]
fn explain_includes_all_spawner_task_names() {
    async fn scenario() {
        let scope = Tasks::<()>::new();
        let spawner = scope.spawner();
        spawner
            .spawn_named("named async", async { Ok(()) })
            .unwrap();
        spawner
            .spawn_handled_named("handled async", async { Ok::<_, ()>(()) }, |()| {})
            .unwrap();
        spawner
            .spawn_blocking_named("named blocking", || Ok(()))
            .unwrap();
        scope.join(Ok(())).await.unwrap();
    }

    let replay = adamantium::testing::check(
        adamantium::testing::Config {
            seed: 224,
            iterations: 20,
            max_steps: 10_000,
        },
        scenario,
    )
    .unwrap();
    let timeline = adamantium::testing::explain(&replay, scenario);
    assert_eq!(timeline.replay(), &replay);
    assert_eq!(timeline.failure(), None);
    for name in ["named async", "handled async", "named blocking"] {
        assert!(
            timeline
                .tasks()
                .iter()
                .any(|task| task.name() == Some(name)),
            "{timeline}"
        );
    }
}

scenario!(children_discover_after_the_body_returns, {
    let completed = Actor::new(0);
    let (release, released) = Reply::pair();
    let mut scope = Box::pin(Tasks::<()>::run(async |tasks| {
        let spawner = tasks.spawner();
        let child_spawner = spawner.clone();
        let completed = completed.clone();
        tasks.spawn(async move {
            released.receive().await.unwrap();
            for _ in 0..4 {
                let sibling_spawner = child_spawner.clone();
                let completed = completed.clone();
                child_spawner
                    .spawn(async move {
                        sibling_spawner
                            .spawn(async move {
                                completed.with(|n| *n += 1);
                                Ok(())
                            })
                            .unwrap();
                        Ok(())
                    })
                    .unwrap();
            }
            Ok(())
        });
        Ok(spawner)
    }));
    // The body has returned and joining is pending on its registered child.
    begin(scope.as_mut()).await;
    release.send(()).unwrap();
    let spawner = scope.await.unwrap();
    assert_eq!(completed.snapshot(), 4);
    assert!(spawner.spawn(async { Ok(()) }).is_err());
});

scenario!(successful_child_cleanup_can_discover_siblings, {
    struct DiscoverOnDrop {
        spawner: Spawner<()>,
        completed: Actor<bool>,
    }
    impl Drop for DiscoverOnDrop {
        fn drop(&mut self) {
            let completed = self.completed.clone();
            self.spawner
                .spawn(async move {
                    completed.with(|done| *done = true);
                    Ok(())
                })
                .unwrap();
        }
    }
    let completed = Actor::new(false);
    let mut scope = Tasks::<()>::new();
    let guard = DiscoverOnDrop {
        spawner: scope.spawner(),
        completed: completed.clone(),
    };
    scope.spawn(async move {
        let _guard = guard;
        Ok(())
    });
    scope.join(Ok(())).await.unwrap();
    assert!(completed.snapshot());
});

scenario!(
    empty_reaping_keeps_admission_open_and_results_reach_the_owner,
    {
        Tasks::<&str>::run(async |tasks| {
            let spawner = tasks.spawner();
            let first = spawner.spawn(async { Ok(42) }).unwrap();
            assert_eq!(tasks.value(first).await, Ok(42));
            // Give the run loop a chance to reap the now empty set.
            tokio::task::yield_now().await;
            let (reply, received) = Reply::pair();
            tasks.spawn(async move {
                let result = spawner.spawn(async { Err::<(), _>("claimed") }).unwrap();
                reply.send(result).unwrap();
                Ok(())
            });
            assert_eq!(
                tasks.value(received.receive().await.unwrap()).await,
                Err("claimed")
            );
            Ok(())
        })
        .await
        .unwrap();
    }
);

scenario!(retained_and_orphaned_errors_keep_their_rules, {
    let tasks = Tasks::<&str>::new();
    let retained = tasks
        .spawner()
        .spawn(async { Err::<(), _>("retained") })
        .unwrap();
    assert_eq!(tasks.join(Ok(())).await, Err("retained"));
    drop(retained);

    let result = Tasks::<&str>::run(async |tasks| {
        tasks
            .spawner()
            .spawn(async { Err::<(), _>("orphaned") })
            .unwrap();
        pending::<Result<(), &str>>().await
    })
    .await;
    assert_eq!(result, Err("orphaned"));
});

#[derive(Clone, Copy, Debug)]
enum End {
    Success,
    Failure,
    Cancellation,
    Deadline,
}

#[derive(Clone, Default)]
struct Observation {
    admitted: Option<bool>,
    started: bool,
    dropped: bool,
}

struct Dropped(Actor<Observation>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.with(|state| state.dropped = true);
    }
}

scenario!(registration_races_completion_and_shutdown, {
    for end in [End::Success, End::Failure, End::Cancellation, End::Deadline] {
        let scope = Tasks::<&str>::new();
        let spawner = scope.spawner();
        let observations: Vec<_> = (0..8).map(|_| Actor::new(Observation::default())).collect();
        let mut producer = Tasks::<()>::new();
        let attempts = observations.clone();
        let late = spawner.clone();
        producer.spawn(async move {
            for observation in attempts {
                let guard = Dropped(observation.clone());
                let started = observation.clone();
                let admitted = late
                    .spawn(async move {
                        let _guard = guard;
                        started.with(|state| state.started = true);
                        if !matches!(end, End::Success) {
                            pending::<()>().await;
                        }
                        Ok(())
                    })
                    .is_ok();
                observation.with(|state| state.admitted = Some(admitted));
                tokio::task::yield_now().await;
            }
            Ok(())
        });
        match end {
            End::Success => assert_eq!(scope.join(Ok(())).await, Ok(())),
            End::Failure => assert_eq!(scope.join(Err::<(), _>("body")).await, Err("body")),
            End::Cancellation => {
                let cancellation = Cancellation::new();
                cancellation.cancel();
                assert_eq!(
                    scope.join_until(cancellation, None, Ok(())).await,
                    Err(ScopeError::Cancelled)
                );
            }
            End::Deadline => {
                assert_eq!(
                    scope
                        .join_until(Cancellation::new(), Some(time::now()), Ok(()))
                        .await,
                    Err(ScopeError::DeadlineExceeded)
                );
            }
        }
        let at_join: Vec<_> = observations
            .iter()
            .map(|state| state.snapshot().dropped)
            .collect();
        producer.join(Ok(())).await.unwrap();
        for (observation, drained) in observations.iter().zip(at_join) {
            let observation = observation.snapshot();
            assert!(observation.dropped);
            match observation.admitted {
                Some(true) => assert!(drained, "accepted work escaped {end:?}"),
                Some(false) => assert!(!observation.started, "rejected work started"),
                None => panic!("producer did not attempt registration"),
            }
        }
        assert!(spawner.spawn(async { Ok(()) }).is_err());
    }
});

struct SpawnOnDrop {
    spawner: Spawner<&'static str>,
    rejected: Actor<usize>,
    done: Option<Reply<()>>,
}

impl Drop for SpawnOnDrop {
    fn drop(&mut self) {
        assert!(
            self.spawner
                .spawn(async {
                    panic!("cleanup work must not start");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .is_err()
        );
        self.rejected.with(|n| *n += 1);
        if let Some(done) = self.done.take() {
            done.send(()).unwrap();
        }
    }
}

scenario!(abort_and_rejection_allow_reentrant_cleanup, {
    let mut scope = Tasks::new();
    let spawner = scope.spawner();
    let rejected = Actor::new(0);
    let guard = SpawnOnDrop {
        spawner: spawner.clone(),
        rejected: rejected.clone(),
        done: None,
    };
    scope.spawn(async move {
        let _guard = guard;
        pending::<Result<(), &str>>().await
    });
    assert_eq!(scope.join(Err::<(), _>("body")).await, Err("body"));
    assert_eq!(rejected.snapshot(), 1);
    let guard = SpawnOnDrop {
        spawner: spawner.clone(),
        rejected: rejected.clone(),
        done: None,
    };
    assert!(
        spawner
            .spawn(async move {
                let _guard = guard;
                panic!("rejected work was polled");
                #[allow(unreachable_code)]
                Ok(())
            })
            .is_err()
    );
    assert_eq!(rejected.snapshot(), 2);
});

scenario!(
    owner_drop_aborts_even_when_children_and_callers_keep_spawners,
    {
        let mut scope = Tasks::new();
        let spawner = scope.spawner();
        let rejected = Actor::new(0);
        let (done, finished) = Reply::pair();
        let guard = SpawnOnDrop {
            spawner: spawner.clone(),
            rejected: rejected.clone(),
            done: Some(done),
        };
        scope.spawn(async move {
            let _guard = guard;
            pending::<Result<(), &str>>().await
        });
        // The unpolled join future still owns shutdown authority.
        let cancellation = Cancellation::new();
        drop(scope.join_until(cancellation.clone(), None, Ok(())));
        assert!(cancellation.is_cancelled());
        assert!(spawner.spawn(async { Ok(()) }).is_err());
        finished.receive().await.unwrap();
        assert_eq!(rejected.snapshot(), 1);
    }
);

#[cfg(not(target_os = "wasi"))]
scenario!(
    started_blocking_work_observes_closed_admission_before_shutdown,
    {
        for named in [false, true] {
            let scope = Tasks::new();
            let spawner = scope.spawner();
            let signal = Cancellation::new();
            let blocking_signal = signal.clone();
            let (started, running) = Reply::pair();
            let finished = Actor::new(false);
            let completed = finished.clone();
            let release = Actor::new(false);
            let may_finish = release.clone();
            let sibling = spawner.clone();
            let work = move || {
                started.send(()).unwrap();
                while !blocking_signal.is_cancelled() {
                    std::thread::yield_now();
                }
                assert!(sibling.spawn(async { Ok(()) }).is_err());
                while !may_finish.snapshot() {
                    std::thread::yield_now();
                }
                completed.with(|done| *done = true);
                Err::<(), _>("blocking child")
            };
            if named {
                spawner
                    .spawn_blocking_named("drained closure", work)
                    .unwrap();
            } else {
                spawner.spawn_blocking(work).unwrap();
            }
            running.receive().await.unwrap();
            let mut join = std::pin::pin!(scope.join_until(signal, None, Err::<(), _>("body")));
            let first_poll = std::future::poll_fn(|cx| Poll::Ready(join.as_mut().poll(cx))).await;
            release.with(|ready| *ready = true);
            assert!(
                first_poll.is_pending(),
                "join returned with blocking work still running"
            );
            assert_eq!(join.await, Err(ScopeError::Failed("body")));
            assert!(finished.snapshot());
        }
    }
);

#[cfg(not(target_os = "wasi"))]
scenario!(sibling_panics_win_after_cleanup, {
    let rejected = Actor::new(0);
    let mut scope = Tasks::new();
    let spawner = scope.spawner();
    let guard = SpawnOnDrop {
        spawner: spawner.clone(),
        rejected: rejected.clone(),
        done: None,
    };
    scope.spawn(async move {
        let _guard = guard;
        pending::<Result<(), &str>>().await
    });
    spawner
        .spawn(async {
            panic!("sibling panic");
            #[allow(unreachable_code)]
            Ok(())
        })
        .unwrap();
    let panic = AssertUnwindSafe(scope.join(Ok(())))
        .catch_unwind()
        .await
        .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"sibling panic"));
    assert_eq!(rejected.snapshot(), 1);
    assert!(spawner.spawn(async { Ok(()) }).is_err());

    struct PanicDrop;
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic!("aborted sibling panic");
        }
    }
    let scope = Tasks::<&str>::new();
    let guard = PanicDrop;
    scope
        .spawner()
        .spawn(async move {
            let _guard = guard;
            pending::<Result<(), &str>>().await
        })
        .unwrap();
    let panic = AssertUnwindSafe(scope.join(Err::<(), _>("body")))
        .catch_unwind()
        .await
        .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"aborted sibling panic"));
});

scenario!(registration_races_owner_drop, {
    let scope = Tasks::<&str>::new();
    let spawner = scope.spawner();
    let observations: Vec<_> = (0..8).map(|_| Actor::new(Observation::default())).collect();
    let mut producer = Tasks::<()>::new();
    let attempts = observations.clone();
    let late = spawner.clone();
    producer.spawn(async move {
        for observation in attempts {
            let guard = Dropped(observation.clone());
            let started = observation.clone();
            let admitted = late
                .spawn(async move {
                    let _guard = guard;
                    started.with(|state| state.started = true);
                    pending::<Result<(), &str>>().await
                })
                .is_ok();
            observation.with(|state| state.admitted = Some(admitted));
            tokio::task::yield_now().await;
        }
        Ok(())
    });
    drop(scope);
    producer.join(Ok(())).await.unwrap();
    for observation in observations {
        // Drop requests abort but, unlike join, cannot await destruction.
        while !observation.snapshot().dropped {
            tokio::task::yield_now().await;
        }
        let observation = observation.snapshot();
        if observation.admitted == Some(false) {
            assert!(!observation.started);
        }
    }
    assert!(spawner.spawn(async { Ok(()) }).is_err());
});

#[cfg(feature = "deterministic")]
#[test]
#[allow(
    clippy::disallowed_types,
    reason = "deliberately pass a handle between executions"
)]
fn handles_reject_foreign_executions() {
    use std::{
        panic::catch_unwind,
        sync::{Arc, Mutex},
    };
    let escaped = Arc::new(Mutex::new(None));
    let output = escaped.clone();
    adamantium::testing::check(
        adamantium::testing::Config {
            seed: 192,
            iterations: 1,
            max_steps: 1_000,
        },
        move || {
            let output = output.clone();
            async move {
                let scope = Tasks::<()>::new();
                *output.lock().unwrap() = Some(scope.spawner());
                scope.join(Ok(())).await.unwrap();
            }
        },
    )
    .unwrap();
    let spawner = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| spawner.clone())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| spawner.spawn(async { Ok(()) }))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(spawner))).is_err());
}

#[cfg(not(target_os = "wasi"))]
#[allow(
    clippy::disallowed_methods,
    reason = "a watchdog must detect synchronous registration deadlocks"
)]
fn completes(work: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        work();
        done.send(()).unwrap();
    });
    finished.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
}

#[cfg(not(target_os = "wasi"))]
#[test]
fn runtime_rejection_destroys_work_outside_the_registration_lock() {
    completes(|| {
        struct Resubmit(Spawner<()>, Actor<bool>);
        impl Drop for Resubmit {
            fn drop(&mut self) {
                // The scope is open, but the runtime will also discard this
                // new task. Its future has no further cleanup to perform.
                self.0.spawn(async { Ok(()) }).unwrap();
                self.1.with(|done| *done = true);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let _entered = handle.enter();
        for kind in ["shared", "body", "handled", "named handled", "blocking"] {
            let mut scope = Tasks::<()>::new();
            let done = Actor::new(false);
            let guard = Resubmit(scope.spawner(), done.clone());
            if kind == "blocking" {
                scope.spawn_blocking(move || {
                    let _guard = guard;
                    Ok(())
                });
            } else {
                let future = async move {
                    let _guard = guard;
                    pending::<Result<(), ()>>().await
                };
                match kind {
                    "shared" => {
                        scope.spawner().spawn(future).unwrap();
                    }
                    "body" => {
                        scope.spawn(future);
                    }
                    "handled" => scope.spawn_handled(future, |()| {}),
                    _ => scope.spawn_handled_named("cleanup", future, |()| {}),
                }
            }
            assert!(done.snapshot(), "work was not destroyed for {kind}");
        }
    });
}

#[cfg(not(target_os = "wasi"))]
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "race a shutdown runtime's registration against joining on a live runtime"
)]
fn join_waits_for_synchronous_registration_cleanup() {
    completes(|| {
        struct Cleanup {
            started: Option<Reply<()>>,
            release: std::sync::mpsc::Receiver<()>,
            finished: Actor<bool>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.started.take().unwrap().send(()).unwrap();
                self.release.recv().unwrap();
                self.finished.with(|done| *done = true);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let scope = Tasks::<()>::new();
        let spawner = scope.spawner();
        let (started, cleaning) = Reply::pair();
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let finished = Actor::new(false);
        let guard = Cleanup {
            started: Some(started),
            release: wait,
            finished: finished.clone(),
        };
        let worker = std::thread::spawn(move || {
            let _entered = handle.enter();
            spawner
                .spawn(async move {
                    let _guard = guard;
                    Ok(())
                })
                .unwrap();
        });
        let live = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        live.block_on(async {
            cleaning.receive().await.unwrap();
            let mut joined = Box::pin(scope.join(Ok(())));
            begin(joined.as_mut()).await;
            release.send(()).unwrap();
            joined.await.unwrap();
            assert!(finished.snapshot());
        });
        worker.join().unwrap();
    });
}

#[cfg(not(target_os = "wasi"))]
#[test]
fn synchronous_cleanup_panics_belong_to_the_scope() {
    completes(|| {
        struct PanicDrop;
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                panic!("registration cleanup panic");
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let live = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for kind in ["shared", "body", "blocking"] {
            let mut scope = Tasks::<&str>::new();
            let guard = PanicDrop;
            {
                let _entered = handle.enter();
                if kind == "blocking" {
                    scope.spawn_blocking(move || {
                        let _guard = guard;
                        Ok(())
                    });
                } else {
                    let future = async move {
                        let _guard = guard;
                        pending::<Result<(), &str>>().await
                    };
                    if kind == "shared" {
                        scope.spawner().spawn(future).unwrap();
                    } else {
                        scope.spawn(future);
                    }
                }
            }
            let payload = live
                .block_on(AssertUnwindSafe(scope.join(Err::<(), _>("body"))).catch_unwind())
                .unwrap_err();
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(&"registration cleanup panic")
            );
        }
    });
}
