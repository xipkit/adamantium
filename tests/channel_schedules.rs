#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    reason = "schedule observations and deliberate execution-boundary misuse"
)]

use std::{
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use adamantium::{
    Channel, Closed, Reply, TryReceiveError, TrySendError,
    testing::{self, Config, FailureKind},
};
use futures_util::{FutureExt, StreamExt};

type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

fn config() -> Config {
    Config {
        seed: 159,
        iterations: 500,
        max_steps: 20_000,
        ..Config::default()
    }
}

#[allow(
    clippy::unwrap_used,
    reason = "test schedule failures must fail the test"
)]
fn check(factory: fn() -> Pin<Box<dyn Future<Output = ()>>>) {
    let record = testing::check(config(), factory).unwrap();
    assert_eq!(testing::check(config(), factory).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

#[test]
fn bounded_mpmc_full_empty_and_last_sender_drop_replay() {
    check(|| {
        Box::pin(async {
            let (sender, receiver) = Channel::bounded(1).split();
            let seen = adamantium::Actor::new(Vec::new());
            let mut workers: Vec<Worker> = Vec::new();
            for producer in 0..2 {
                let sender = sender.clone();
                workers.push(Box::pin(async move {
                    for item in 0..3 {
                        sender.send(producer * 3 + item).await.unwrap();
                    }
                }));
            }
            for _ in 0..2 {
                let receiver = receiver.clone();
                let seen = seen.clone();
                workers.push(Box::pin(async move {
                    while let Some(value) = receiver.receive().await {
                        seen.with(|seen| seen.push(value));
                    }
                }));
            }
            drop(sender);
            drop(receiver);
            testing::parallel(workers).await;
            let mut seen = seen.into_inner().ok().unwrap();
            seen.sort_unstable();
            assert_eq!(seen, (0..6).collect::<Vec<_>>());
        })
    });
}

#[test]
fn fifo_stream_and_explicit_close_drain_replay() {
    check(|| {
        Box::pin(async {
            let (sender, mut receiver) = Channel::bounded(2).split();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    for n in 0..4 {
                        sender.send(n).await.unwrap();
                    }
                    assert!(sender.close());
                    assert!(!sender.close());
                    assert_eq!(sender.send(9).await, Err(Closed(9)));
                }),
                Box::pin(async move {
                    let mut values = Vec::new();
                    while let Some(value) = receiver.next().await {
                        values.push(value);
                    }
                    assert_eq!(values, [0, 1, 2, 3]);
                    assert_eq!(receiver.next().await, None);
                }),
            ];
            testing::parallel(workers).await;
        })
    });
}

#[test]
fn close_racing_send_returns_exactly_the_unsent_value() {
    check(|| {
        Box::pin(async {
            let channel = Channel::bounded(1);
            channel.try_send(1).unwrap();
            let closer = channel.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    assert_eq!(channel.send(2).await, Err(Closed(2)));
                    assert_eq!(channel.try_receive(), Ok(1));
                    assert_eq!(channel.try_receive(), Err(TryReceiveError::Closed));
                }),
                Box::pin(async move {
                    closer.close();
                }),
            ];
            testing::parallel(workers).await;
        })
    });
}

#[test]
fn last_receiver_drop_wakes_every_blocked_sender() {
    check(|| {
        Box::pin(async {
            let (sender, receiver) = Channel::bounded(1).split();
            sender.try_send(0).unwrap();
            let other = sender.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    assert_eq!(sender.send(1).await, Err(Closed(1)));
                }),
                Box::pin(async move {
                    assert_eq!(other.send(2).await, Err(Closed(2)));
                }),
                Box::pin(async move {
                    drop(receiver);
                }),
            ];
            testing::parallel(workers).await;
        })
    });
}

#[test]
fn last_receiver_drop_ends_queued_and_blocked_replies_while_senders_remain() {
    check(|| {
        Box::pin(async {
            let (requests, mailbox) = Channel::bounded(1).split();
            let (queued, queued_answer) = Reply::<u8>::pair();
            requests.try_send(queued).unwrap();
            let (blocked, blocked_answer) = Reply::<u8>::pair();
            let other = requests.clone();
            let observer = requests.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    assert_eq!(queued_answer.receive().await, None);
                    // Ending the reply must not wait for this sender to drop.
                    assert!(requests.is_closed());
                    drop(requests);
                }),
                Box::pin(async move {
                    let Err(Closed(blocked)) = other.send(blocked).await else {
                        panic!("nothing receives, so the send must fail");
                    };
                    drop(blocked);
                    assert_eq!(blocked_answer.receive().await, None);
                }),
                Box::pin(async move {
                    drop(mailbox);
                }),
            ];
            testing::parallel(workers).await;
            assert!(observer.is_closed());
            assert!(observer.is_empty());
        })
    });
}

#[test]
fn cancelled_receive_hands_wakeup_to_competing_receiver() {
    check(|| {
        Box::pin(async {
            let (sender, receiver) = Channel::bounded(1).split();
            let mut cancelled = Box::pin(receiver.receive());
            poll_fn(|cx| {
                assert!(cancelled.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let other = receiver.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    sender.send(7).await.unwrap();
                }),
                Box::pin(async move {
                    assert_eq!(other.receive().await, Some(7));
                }),
            ];
            // The cancellation can race the producer's notification after spawning.
            let workers = testing::parallel(workers);
            let cancel = async {
                drop(cancelled);
            };
            futures_util::future::join(workers, cancel).await;
        })
    });
}

#[test]
fn cancelled_send_hands_room_to_competing_sender() {
    check(|| {
        Box::pin(async {
            let (sender, receiver) = Channel::bounded(1).split();
            sender.try_send(0).unwrap();
            let mut cancelled = Box::pin(sender.send(99));
            poll_fn(|cx| {
                assert!(cancelled.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let other = sender.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    assert_eq!(other.send(1).await, Ok(()));
                }),
                Box::pin(async move {
                    assert_eq!(receiver.receive().await, Some(0));
                    assert_eq!(receiver.receive().await, Some(1));
                    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Empty));
                }),
            ];
            let workers = testing::parallel(workers);
            let cancel = async {
                drop(cancelled);
            };
            futures_util::future::join(workers, cancel).await;
        })
    });
}

#[test]
fn select_cancellation_races_ready_message_without_losing_it() {
    check(|| {
        Box::pin(async {
            let values = Channel::bounded(1);
            let stop = Channel::bounded(1);
            let reader = values.clone();
            let signal = stop.clone();
            let taken = adamantium::Actor::new(None);
            let outcome = taken.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "exercise dropping a pending receive when it loses a select"
                    )]
                    let () = tokio::select! {
                        biased;
                        _ = signal.receive() => {},
                        value = reader.receive() => outcome.with(|out| *out = value),
                    };
                }),
                Box::pin(async move {
                    stop.send(()).await.unwrap();
                }),
                Box::pin({
                    let writer = values.clone();
                    async move {
                        writer.send(7).await.unwrap();
                    }
                }),
            ];
            testing::parallel(workers).await;
            match taken.snapshot() {
                Some(7) => assert_eq!(values.try_receive(), Err(TryReceiveError::Empty)),
                None => assert_eq!(values.try_receive(), Ok(7)),
                other => panic!("unexpected value: {other:?}"),
            }
        })
    });
}

#[test]
fn close_and_sync_observations_have_both_schedules_and_failure_replays() {
    let outcomes = Arc::new(Mutex::new([false; 2]));
    let out = outcomes.clone();
    testing::check(config(), move || {
        let out = out.clone();
        async move {
            let channel = Channel::bounded(1);
            let closer = channel.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    closer.close();
                }),
                Box::pin(async move {
                    let sent = channel.try_send(1).is_ok();
                    out.lock().unwrap()[usize::from(sent)] = true;
                }),
            ];
            testing::parallel(workers).await;
        }
    })
    .unwrap();
    assert_eq!(*outcomes.lock().unwrap(), [true, true]);

    async fn bad_assumption() {
        let channel = Channel::<u8>::bounded(1);
        let closer = channel.clone();
        let workers: Vec<Worker> = vec![
            Box::pin(async move {
                closer.close();
            }),
            Box::pin(async move {
                assert!(!channel.is_closed());
            }),
        ];
        testing::parallel(workers).await;
    }
    let failure = testing::check(config(), bad_assumption).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    let replayed = testing::replay(failure.replay(), bad_assumption).unwrap_err();
    assert_eq!(replayed.kind(), FailureKind::Panic);
    assert_eq!(replayed.replay(), failure.replay());
}

#[test]
fn production_handles_reject_every_sync_operation_and_drop() {
    type Operation = fn(Channel<u8>);
    let operations: Vec<Operation> = vec![
        |c| {
            c.try_send(1).unwrap();
        },
        |c| {
            assert!(c.try_receive().is_err());
        },
        |c| {
            c.close();
        },
        |c| {
            c.is_closed();
        },
        |c| {
            c.len();
        },
        |c| {
            c.is_empty();
        },
        |c| {
            c.capacity();
        },
        |c| {
            drop(c.clone());
        },
        |c| {
            drop(c.split());
        },
        |c| {
            drop(format!("{c:?}"));
        },
        drop,
    ];
    for operation in operations {
        let value = Mutex::new(Some(Channel::bounded(1)));
        let failure = testing::check(
            Config {
                iterations: 1,
                ..config()
            },
            move || {
                let value = value.lock().unwrap().take().unwrap();
                async move {
                    operation(value);
                }
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Panic);
    }
}

#[test]
fn escaped_handles_and_registered_futures_reject_other_executions() {
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
                *out.lock().unwrap() = Some(Channel::bounded(1));
            }
        },
    )
    .unwrap();
    let channel = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| channel.try_send(1))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(channel))).is_err());

    // Cancellation of a registered production future is itself an owned operation.
    let sender = Arc::new(Channel::bounded(1));
    sender.try_send(1).unwrap();
    let writer = sender.clone();
    let mut send = Box::pin(async move { writer.send(2).await });
    assert!(send.as_mut().now_or_never().is_none());
    let send = Mutex::new(Some(send));
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let send = send.lock().unwrap().take();
            async move {
                drop(send);
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(sender.try_receive(), Ok(1));
    assert_eq!(sender.try_send(3), Ok(()));
    assert_eq!(sender.try_send(4), Err(TrySendError::Full(4)));
}

#[test]
fn dropping_last_sender_while_unwinding_closes_the_queue() {
    check(|| {
        Box::pin(async {
            let (sender, receiver) = Channel::<u8>::bounded(1).split();
            assert!(
                catch_unwind(AssertUnwindSafe(move || {
                    let _sender = sender;
                    panic!("caught failure");
                }))
                .is_err()
            );
            assert_eq!(receiver.receive().await, None);
        })
    });
}

#[test]
fn a_channel_cannot_be_reused_in_the_next_execution() {
    let held = Arc::new(Mutex::new(None));
    let retained = held.clone();
    let failure = testing::check(config(), move || {
        let retained = retained.clone();
        async move {
            let mut retained = retained.lock().unwrap();
            match &*retained {
                Some(channel) => {
                    Channel::<u8>::is_closed(channel);
                }
                None => {
                    *retained = Some(Channel::<u8>::bounded(1));
                }
            }
        }
    })
    .unwrap_err();
    assert_eq!(failure.replay().iteration(), 2);
    assert_eq!(failure.kind(), FailureKind::Panic);
    let channel = held.lock().unwrap_or_else(|p| p.into_inner()).take();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(channel))).is_err());
}

#[test]
fn cancelled_next_retained_during_active_receive_replays() {
    check(|| {
        Box::pin(async {
            for stream in [false, true] {
                let (sender, mut idle) = Channel::bounded(1).split();
                poll_fn(|cx| {
                    assert!(std::pin::pin!(idle.next()).poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let mut active = idle.clone();
                let producer = sender.clone();
                let workers: Vec<Worker> = vec![
                    Box::pin(async move {
                        producer.send(7).await.unwrap();
                    }),
                    Box::pin(async move {
                        let value = if stream {
                            active.next().await
                        } else {
                            active.receive().await
                        };
                        assert_eq!(value, Some(7));
                    }),
                ];
                testing::parallel(workers).await;
                assert!(!sender.is_closed());
                assert_eq!(idle.try_receive(), Err(TryReceiveError::Empty));
            }
        })
    });
}

#[test]
fn parked_receive_reports_deadlock_instead_of_waking_itself_forever() {
    async fn scenario() {
        Channel::<()>::bounded(1).receive().await;
    }
    let failure = testing::check(config(), scenario).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Deadlock);
    let replayed = testing::replay(failure.replay(), scenario).unwrap_err();
    assert_eq!(replayed.kind(), FailureKind::Deadlock);
    assert_eq!(replayed.replay(), failure.replay());
}
