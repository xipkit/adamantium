#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "deterministic schedule observations and assertions"
)]

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use adamantium::{
    Actor, AsyncActor, Cancellation, Closed, ReceiveError, Reply, SendError, Tasks, TrySendError,
    race,
    testing::{self, Config, FailureKind, SchedulerMode, TimelineEventKind},
    time,
};

fn config() -> Config {
    Config {
        seed: 226,
        iterations: 300,
        max_steps: 20_000,
        ..Config::default()
    }
}

#[allow(
    clippy::unwrap_used,
    reason = "deterministic test failures must fail the test"
)]
fn check(factory: fn() -> Pin<Box<dyn Future<Output = ()>>>) {
    let record = testing::check(config(), factory).unwrap();
    assert_eq!(testing::check(config(), factory).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

#[test]
fn new_named_worker_appears_in_explain_timelines_without_perturbation() {
    async fn deadlocking_named_actor() {
        let mut tasks = Tasks::<()>::new();
        let actor = AsyncActor::new_named(
            &mut tasks,
            "order-actor",
            (),
            1,
            |(), (id, reply): (u32, Reply<u32>)| {
                Box::pin(async move {
                    time::sleep(Duration::from_millis(5)).await;
                    reply.send(id + 1).ok();
                    Ok(())
                })
            },
        );
        let caller_actor = actor.clone();
        tasks.spawn_named("order-caller", async move {
            let answer = caller_actor.call(|reply| (10, reply)).await;
            assert_eq!(answer, Some(11));
            // Deliberately forget `actor.close()` so the worker stays waiting in the scope.
            Ok(())
        });
        tasks.join(Ok(())).await.unwrap();
    }

    for mode in [
        SchedulerMode::Random,
        SchedulerMode::Pct { depth: 3 },
        SchedulerMode::Dfs,
    ] {
        let exploration = Config {
            seed: 226,
            iterations: 20,
            max_steps: 5_000,
            ..Config::default()
        }
        .with_scheduler(mode);

        let failure = testing::check(exploration, deadlocking_named_actor).unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Deadlock);

        let untraced = testing::replay(failure.replay(), deadlocking_named_actor).unwrap_err();
        assert_eq!(untraced.kind(), FailureKind::Deadlock);
        assert_eq!(untraced.replay(), failure.replay());

        let timeline = testing::explain(failure.replay(), deadlocking_named_actor);
        assert_eq!(timeline.failure(), Some(FailureKind::Deadlock));
        assert_eq!(timeline.replay(), failure.replay());

        let task_names: Vec<&str> = timeline
            .tasks()
            .iter()
            .filter_map(|task| task.name())
            .collect();
        assert!(task_names.contains(&"order-actor"), "{task_names:?}");
        assert!(task_names.contains(&"order-caller"), "{task_names:?}");

        let saw_actor_wakeup = timeline.events().iter().any(|event| {
            matches!(
                event.kind(),
                TimelineEventKind::Wakeup {
                    by_task_name: Some(by),
                    operation: "channel.receive",
                    target_task_names,
                    ..
                } if by == "order-actor"
                    && target_task_names
                        .iter()
                        .any(|name| name.as_deref() == Some("order-caller"))
            )
        });
        assert!(saw_actor_wakeup, "{timeline}");
    }
}

#[test]
fn try_send_and_send_until_conserve_messages_under_close_cancel_and_deadline() {
    check(|| {
        Box::pin(async {
            let processed = Actor::new(Vec::new());
            let returned = Actor::new(Vec::new());
            let cancel = Cancellation::new();
            let result = Tasks::<&str>::run(async |tasks| {
                let actor =
                    AsyncActor::new_named(tasks, "sink", processed.clone(), 1, |log, id: usize| {
                        Box::pin(async move {
                            time::sleep(Duration::from_millis(2)).await;
                            log.with(|items| items.push(id));
                            Ok(())
                        })
                    });

                // Producer 1 uses try_send.
                let a1 = actor.clone();
                let r1 = returned.clone();
                tasks.spawn_named("try-sender", async move {
                    for id in [0, 1, 2] {
                        match a1.try_send(id) {
                            Ok(()) => {}
                            Err(TrySendError::Full(m) | TrySendError::Closed(m)) => {
                                r1.with(|items| items.push(m));
                            }
                        }
                    }
                    Ok(())
                });

                // Producer 2 uses send_until with cancellation and a virtual deadline.
                let a2 = actor.clone();
                let r2 = returned.clone();
                let c2 = cancel.clone();
                let deadline = time::now() + Duration::from_millis(3);
                tasks.spawn_named("until-sender", async move {
                    for id in [3, 4] {
                        match a2.send_until(&c2, Some(deadline), id).await {
                            Ok(()) => {}
                            Err(
                                SendError::Closed(m)
                                | SendError::Cancelled(m)
                                | SendError::DeadlineExceeded(m),
                            ) => {
                                r2.with(|items| items.push(m));
                            }
                        }
                    }
                    Ok(())
                });

                // Canceller + closer racing with producers.
                let c3 = cancel.clone();
                let a3 = actor.clone();
                tasks.spawn_named("stopper", async move {
                    time::sleep(Duration::from_millis(2)).await;
                    c3.cancel();
                    a3.close();
                    Ok(())
                });

                Ok(())
            })
            .await;
            assert_eq!(result, Ok(()));

            let mut all = processed.snapshot();
            let rejected = returned.snapshot();
            all.extend_from_slice(&rejected);
            all.sort_unstable();
            assert_eq!(all, vec![0, 1, 2, 3, 4]);
        })
    });
}

#[test]
fn call_and_call_until_under_close_and_handler_failure_replay() {
    check(|| {
        Box::pin(async {
            // 1. Concurrent call / call_until racing with close().
            let outcome = Tasks::<&str>::run(async |tasks| {
                let actor = AsyncActor::new_named(
                    tasks,
                    "echo",
                    (),
                    1,
                    |(), (id, reply): (usize, Reply<usize>)| {
                        Box::pin(async move {
                            time::sleep(Duration::from_millis(1)).await;
                            reply.send(id * 10).ok();
                            Ok(())
                        })
                    },
                );
                let open = Cancellation::new();
                let a1 = actor.clone();
                let t1 = tasks.spawn_named("caller-1", async move {
                    if let Some(v) = a1.call(|reply| (1, reply)).await {
                        assert_eq!(v, 10);
                    }
                    Ok(())
                });
                let a2 = actor.clone();
                let t2 = tasks.spawn_named("caller-2", async move {
                    match a2.call_until(&open, None, |reply| (2, reply)).await {
                        Ok(v) => assert_eq!(v, 20),
                        Err(ReceiveError::Closed) => {}
                        Err(other) => panic!("unexpected error: {other:?}"),
                    }
                    Ok(())
                });
                actor.close();
                tasks.value(t1).await?;
                tasks.value(t2).await?;
                Ok(())
            })
            .await;
            assert_eq!(outcome, Ok(()));

            // 2. Concurrent calls racing with a handler error.
            let outcome = Tasks::<&str>::run(async |tasks| {
                let actor = AsyncActor::new_named(
                    tasks,
                    "flaky",
                    (),
                    1,
                    |(), (fail, reply): (bool, Reply<u32>)| {
                        Box::pin(async move {
                            if fail {
                                return Err("handler failed");
                            }
                            reply.send(7).ok();
                            Ok(())
                        })
                    },
                );
                let open = Cancellation::new();
                let a1 = actor.clone();
                let t1 = tasks.spawn_named("failing-call", async move {
                    assert_eq!(a1.call(|reply| (true, reply)).await, None);
                    Ok(())
                });
                let a2 = actor.clone();
                let t2 = tasks.spawn_named("sibling-call", async move {
                    match a2.call_until(&open, None, |reply| (false, reply)).await {
                        Ok(7) | Err(ReceiveError::Closed) => Ok(()),
                        other => panic!("unexpected result: {other:?}"),
                    }
                });
                tasks.value(t1).await?;
                tasks.value(t2).await?;
                actor.close();
                Ok(())
            })
            .await;
            assert_eq!(outcome, Err("handler failed"));
        })
    });
}

#[test]
fn call_until_cancellation_and_deadline_interleavings_notify_handler_and_keep_actor_alive() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HandlerObservation {
        Sent,
        GaveUpAfterAccept,
    }

    let saw_ok = Arc::new(AtomicBool::new(false));
    let saw_gave_up = Arc::new(AtomicBool::new(false));
    let saw_cancelled_or_deadline = Arc::new(AtomicBool::new(false));

    let ok_flag = saw_ok.clone();
    let gave_up_flag = saw_gave_up.clone();
    let timeout_flag = saw_cancelled_or_deadline.clone();

    let scenario = move || {
        let ok_flag = ok_flag.clone();
        let gave_up_flag = gave_up_flag.clone();
        let timeout_flag = timeout_flag.clone();
        async move {
            for use_deadline in [false, true] {
                let observations = Actor::new(Vec::<(u32, HandlerObservation)>::new());
                let result = Tasks::<&str>::run(async |tasks| {
                    let obs = observations.clone();
                    let actor = AsyncActor::new_named(
                        tasks,
                        "worker",
                        obs,
                        1,
                        |obs, (id, slow, reply): (u32, bool, Reply<u32>)| {
                            Box::pin(async move {
                                if !slow {
                                    reply.send(id * 10).unwrap();
                                    return Ok(());
                                }
                                let abandoned = if reply.is_closed() {
                                    true
                                } else {
                                    race((
                                        async {
                                            reply.closed().await;
                                            true
                                        },
                                        async {
                                            time::sleep(Duration::from_millis(5)).await;
                                            false
                                        },
                                    ))
                                    .await
                                };
                                if abandoned {
                                    assert!(reply.is_closed());
                                    assert_eq!(reply.send(id * 10), Err(Closed(id * 10)));
                                    obs.with(|log| {
                                        log.push((id, HandlerObservation::GaveUpAfterAccept));
                                    });
                                } else {
                                    match reply.send(id * 10) {
                                        Ok(()) => {
                                            obs.with(|log| {
                                                log.push((id, HandlerObservation::Sent));
                                            });
                                        }
                                        Err(Closed(value)) => {
                                            assert_eq!(value, id * 10);
                                            obs.with(|log| {
                                                log.push((
                                                    id,
                                                    HandlerObservation::GaveUpAfterAccept,
                                                ));
                                            });
                                        }
                                    }
                                }
                                Ok(())
                            })
                        },
                    );

                    let cancel = Cancellation::new();
                    let deadline = use_deadline.then(|| time::now() + Duration::from_millis(5));
                    if !use_deadline {
                        let c = cancel.clone();
                        tasks.spawn_named("canceller", async move {
                            time::sleep(Duration::from_millis(5)).await;
                            c.cancel();
                            Ok(())
                        });
                    }

                    let outcome = actor
                        .call_until(&cancel, deadline, |reply| (1, true, reply))
                        .await;
                    // Follow-up call proves the actor remained alive after caller give-up.
                    assert_eq!(actor.call(|reply| (2, false, reply)).await, Some(20));
                    actor.close();

                    let log = observations.snapshot();
                    match outcome {
                        Ok(value) => {
                            assert_eq!(value, 10);
                            assert_eq!(log, vec![(1, HandlerObservation::Sent)]);
                            ok_flag.store(true, Ordering::SeqCst);
                        }
                        Err(ReceiveError::Cancelled) if !use_deadline => {
                            timeout_flag.store(true, Ordering::SeqCst);
                            // Either never accepted, accepted and handler saw give-up, or
                            // handler sent just before cancellation won.
                            assert!(log.len() <= 1);
                            if let Some(&(_, HandlerObservation::GaveUpAfterAccept)) = log.first() {
                                gave_up_flag.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(ReceiveError::DeadlineExceeded) if use_deadline => {
                            timeout_flag.store(true, Ordering::SeqCst);
                            assert!(log.len() <= 1);
                            if let Some(&(_, HandlerObservation::GaveUpAfterAccept)) = log.first() {
                                gave_up_flag.store(true, Ordering::SeqCst);
                            }
                        }
                        other => panic!("unexpected call_until outcome: {other:?}"),
                    }
                    Ok(())
                })
                .await;
                assert_eq!(result, Ok(()));
            }
        }
    };

    let record = testing::check(config(), scenario.clone()).unwrap();
    assert_eq!(testing::replay(&record, scenario).unwrap(), record);
    assert!(
        saw_ok.load(Ordering::SeqCst),
        "expected some calls to complete"
    );
    assert!(
        saw_cancelled_or_deadline.load(Ordering::SeqCst),
        "expected some calls to end on cancellation or deadline"
    );
    assert!(
        saw_gave_up.load(Ordering::SeqCst),
        "expected handler to observe caller give-up after acceptance"
    );
}
