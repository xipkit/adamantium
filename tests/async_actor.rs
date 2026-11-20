#![cfg(feature = "tokio")]

mod support;

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

use adamantium::{
    Actor, AsyncActor, Cancellation, Channel, Closed, ReceiveError, Reply, ScopeError, SendError,
    Str, Tasks, TrySendError, race, time,
};
use futures_util::FutureExt;
use support::timeout;
use tokio::io::AsyncReadExt;

fn past() -> Option<tokio::time::Instant> {
    Some(time::now() - Duration::from_millis(1))
}

const HUNG: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Message {
    id: usize,
    dropped: Arc<AtomicUsize>,
}

impl Drop for Message {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn message(id: usize, dropped: &Arc<AtomicUsize>) -> Message {
    Message {
        id,
        dropped: dropped.clone(),
    }
}

async fn pending<F: Future + Send>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

macro_rules! on_both_runtimes {
    ($($(#[$attribute:meta])* async fn $name:ident() $body:block)*) => {$(
        mod $name {
            use super::*;

            $(#[$attribute])*
            #[tokio::test]
            async fn current_thread() $body

            #[cfg(not(target_os = "wasi"))]
            #[tokio::test(flavor = "multi_thread")]
            async fn multi_thread() $body
        }
    )*};
}

on_both_runtimes! {
    async fn concurrent_producers_preserve_each_producers_order() {
        let completed = Actor::new(Vec::new());
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(&mut tasks, completed.clone(), 2,
            |state, message: (usize, usize)| Box::pin(async move {
                tokio::task::yield_now().await;
                state.with(|log| log.push(message));
                Ok(())
            }));
        let mut producers = Vec::new();
        for producer in 0..8 {
            let mailbox = actor.clone();
            producers.push(tasks.spawn(async move {
                for sequence in 0..16 {
                    mailbox.send((producer, sequence)).await.map_err(|_| "closed")?;
                }
                Ok(())
            }));
        }
        for producer in producers {
            timeout(HUNG, tasks.value(producer)).await.unwrap().unwrap();
        }
        actor.close();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        let completed = completed.snapshot();
        assert_eq!(completed.len(), 128);
        for producer in 0..8 {
            assert_eq!(completed.iter().filter(|(id, _)| *id == producer)
                .map(|(_, sequence)| *sequence).collect::<Vec<_>>(),
                (0..16).collect::<Vec<_>>());
        }
    }

    async fn sequential_handling_backpressure_and_close_drain() {
        let started = Channel::bounded(4);
        let release = Channel::bounded(4);
        let completed = Actor::new(Vec::new());
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks, (started.clone(), release.clone(), completed.clone(), 0), 1,
            |state, id| Box::pin(async move {
                assert_eq!(id, state.3);
                state.0.send(id).await.unwrap();
                state.1.receive().await.unwrap();
                state.2.with(|log| log.push(id));
                state.3 += 1;
                Ok(())
            }),
        );
        assert_eq!(tasks.len(), 1);
        actor.send(0).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));
        actor.send(1).await.unwrap();
        let producer = actor.clone();
        let mut waiting = Box::pin(producer.send(2));
        pending(waiting.as_mut()).await;
        assert!(completed.read(Vec::is_empty));
        assert!(started.is_empty());
        release.send(()).await.unwrap();
        timeout(HUNG, waiting).await.unwrap().unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));
        assert_eq!(completed.snapshot(), vec![0]);
        let mut rejected = Box::pin(producer.send(3));
        pending(rejected.as_mut()).await;
        assert!(actor.close());
        assert!(!producer.close());
        assert!(producer.is_closed());
        assert_eq!(timeout(HUNG, rejected).await.unwrap(), Err(Closed(3)));
        assert_eq!(actor.send(4).await, Err(Closed(4)));
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(2));
        release.send(()).await.unwrap();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        assert_eq!(completed.snapshot(), vec![0, 1, 2]);
    }

    async fn worker_failure_wakes_senders_and_releases_state_and_queue() {
        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks, (started.clone(), release.clone(), message(99, &dropped)), 1,
            |state, message: Message| Box::pin(async move {
                state.0.send(()).await.unwrap();
                state.1.receive().await.unwrap();
                drop(message);
                Err("handler failed")
            }),
        );
        actor.send(message(0, &dropped)).await.unwrap();
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        actor.send(message(1, &dropped)).await.unwrap();
        let mut waiting = Box::pin(actor.send(message(2, &dropped)));
        pending(waiting.as_mut()).await;
        release.send(()).await.unwrap();
        let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
        assert_eq!(rejected.id, 2);
        assert!(Arc::ptr_eq(&rejected.dropped, &dropped));
        assert_eq!(timeout(HUNG, tasks.join(Ok(()))).await.unwrap(), Err("handler failed"));
        assert!(actor.is_closed());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        drop(rejected);
        assert_eq!(dropped.load(Ordering::SeqCst), 4);
    }

    async fn a_handler_error_stops_a_running_scope_body() {
        let result = timeout(HUNG, Tasks::<&str>::run(async |tasks| {
            let actor = AsyncActor::new(tasks, (), 1, |(), (): ()| {
                Box::pin(async { Err("handler failed") })
            });
            actor.send(()).await.unwrap();
            std::future::pending::<()>().await;
            Ok(())
        }))
        .await
        .unwrap();
        assert_eq!(result, Err("handler failed"));
    }

    #[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
    async fn panic_closes_mailbox_and_releases_queue() {
        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks, (started.clone(), release.clone()), 1,
            |state, message: Message| Box::pin(async move {
                state.0.send(()).await.unwrap();
                state.1.receive().await.unwrap();
                drop(message);
                panic!("handler panic");
            }),
        );
        actor.send(message(0, &dropped)).await.unwrap();
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        actor.send(message(1, &dropped)).await.unwrap();
        let mut waiting = Box::pin(actor.send(message(2, &dropped)));
        pending(waiting.as_mut()).await;
        release.send(()).await.unwrap();
        let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
        assert_eq!(rejected.id, 2);
        let panic = timeout(HUNG, std::panic::AssertUnwindSafe(tasks.join(Ok(()))).catch_unwind())
            .await.unwrap().unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"handler panic"));
        assert!(actor.is_closed());
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    async fn cancellation_interrupts_io_and_wakes_blocked_senders() {
        let (connection, _peer) = tokio::io::duplex(1);
        let started = Channel::bounded(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let cancellation = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks, (connection, started.clone(), message(99, &dropped)), 1,
            |state, message: Message| Box::pin(async move {
                state.1.send(()).await.unwrap();
                state.0.read_u8().await.unwrap();
                drop(message);
                Ok(())
            }),
        );
        actor.send(message(0, &dropped)).await.unwrap();
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        actor.send(message(1, &dropped)).await.unwrap();
        let mut waiting = Box::pin(actor.send(message(2, &dropped)));
        pending(waiting.as_mut()).await;
        cancellation.cancel();
        assert_eq!(timeout(HUNG, tasks.join_until(cancellation, None, Ok(()))).await.unwrap(),
            Err(ScopeError::Cancelled));
        assert!(actor.is_closed());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
        assert_eq!(rejected.id, 2);
        drop(rejected);
        assert_eq!(dropped.load(Ordering::SeqCst), 4);
    }

    async fn dropping_a_pending_send_never_delivers_its_message() {
        let started = Channel::bounded(1);
        let release = Channel::bounded(2);
        let completed = Actor::new(Vec::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks, (started.clone(), release.clone(), completed.clone()), 1,
            |state, message: Message| Box::pin(async move {
                state.0.send(()).await.unwrap();
                state.1.receive().await.unwrap();
                state.2.with(|log| log.push(message.id));
                drop(message);
                Ok(())
            }),
        );
        actor.send(message(0, &dropped)).await.unwrap();
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        actor.send(message(1, &dropped)).await.unwrap();
        let mut waiting = Box::pin(actor.send(message(2, &dropped)));
        pending(waiting.as_mut()).await;
        drop(waiting);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        actor.close();
        release.send(()).await.unwrap();
        release.send(()).await.unwrap();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        assert_eq!(completed.snapshot(), vec![0, 1]);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    async fn dropping_mailbox_handles_requires_explicit_shutdown() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let cancellation = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(&mut tasks, message(99, &dropped), 0,
            |_, (): ()| Box::pin(async { Ok(()) }));
        drop(actor);
        let mut joined = Box::pin(tasks.join_until(cancellation.clone(), None, Ok(())));
        pending(joined.as_mut()).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        cancellation.cancel();
        assert_eq!(timeout(HUNG, joined).await.unwrap(), Err(ScopeError::Cancelled));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cancellation_before_first_poll_closes_and_discards_queue() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut tasks = Tasks::<&str>::new();
    let actor = AsyncActor::new(&mut tasks, message(99, &dropped), 0, |_, _: Message| {
        Box::pin(async { panic!("worker must not start") })
    });
    actor.send(message(0, &dropped)).await.unwrap();
    let mut waiting = Box::pin(actor.send(message(1, &dropped)));
    pending(waiting.as_mut()).await;
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert_eq!(
        timeout(HUNG, tasks.join_until(cancellation, None, Ok(())))
            .await
            .unwrap(),
        Err(ScopeError::Cancelled)
    );
    assert!(actor.is_closed());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
    assert_eq!(rejected.id, 1);
}

#[tokio::test]
async fn dropping_scope_before_first_poll_also_cleans_up() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut tasks = Tasks::<&str>::new();
    let actor = AsyncActor::new(&mut tasks, message(99, &dropped), 1, |_, _: Message| {
        Box::pin(async { panic!("worker must not start") })
    });
    actor.send(message(0, &dropped)).await.unwrap();
    let mut waiting = Box::pin(actor.send(message(1, &dropped)));
    pending(waiting.as_mut()).await;
    drop(tasks);
    let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
    assert_eq!(rejected.id, 1);
    assert!(actor.is_closed());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn new_named_processes_messages_and_propagates_errors() {
    let seen = Actor::new(Vec::new());
    let result = timeout(
        HUNG,
        Tasks::<&str>::run(async |tasks| {
            let log = seen.clone();
            let first =
                AsyncActor::new_named(tasks, "counter", 0u32, 2, move |total, delta: u32| {
                    let log = log.clone();
                    Box::pin(async move {
                        *total += delta;
                        log.with(|items| items.push(*total));
                        Ok(())
                    })
                });
            let failing =
                AsyncActor::new_named(tasks, Str::from("failing"), (), 1, |(), (): ()| {
                    Box::pin(async { Err("named failed") })
                });
            first.send(3).await.unwrap();
            first.send(4).await.unwrap();
            first.close();
            failing.send(()).await.unwrap();
            std::future::pending::<()>().await;
            Ok(())
        }),
    )
    .await
    .unwrap();
    assert_eq!(result, Err("named failed"));
    let snapshot = seen.snapshot();
    assert!(snapshot.is_empty() || snapshot == [3] || snapshot == [3, 7]);

    // Named actor with String name completes cleanly when closed.
    let result = timeout(
        HUNG,
        Tasks::<&str>::run(async |tasks| {
            let actor = AsyncActor::new_named(
                tasks,
                String::from("adder"),
                10u32,
                2,
                |total, (delta, reply): (u32, Reply<u32>)| {
                    Box::pin(async move {
                        *total += delta;
                        reply.send(*total).ok();
                        Ok(())
                    })
                },
            );
            let sum = actor.call(|reply| (5, reply)).await.unwrap();
            actor.close();
            Ok(sum)
        }),
    )
    .await
    .unwrap();
    assert_eq!(result, Ok(15));
}

#[cfg(not(feature = "tracing"))]
#[tokio::test]
async fn new_named_does_not_convert_name_without_tracing() {
    struct Unconverted;

    impl From<Unconverted> for Str {
        fn from(_: Unconverted) -> Self {
            panic!("a disabled actor name must never be converted")
        }
    }

    let result = Tasks::<&str>::run(async |tasks| {
        let actor = AsyncActor::new_named(
            tasks,
            Unconverted,
            0u32,
            1,
            |state, (n, reply): (u32, Reply<u32>)| {
                Box::pin(async move {
                    *state += n;
                    reply.send(*state).ok();
                    Ok(())
                })
            },
        );
        let value = actor.call(|reply| (7, reply)).await.unwrap();
        actor.close();
        Ok(value)
    })
    .await;
    assert_eq!(result, Ok(7));
}

#[tokio::test]
async fn new_named_cancellation_before_first_poll_closes_and_discards_queue() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut tasks = Tasks::<&str>::new();
    let actor = AsyncActor::new_named(
        &mut tasks,
        "unpolled",
        message(99, &dropped),
        1,
        |_, _: Message| Box::pin(async { panic!("worker must not start") }),
    );
    actor.try_send(message(0, &dropped)).unwrap();
    let mut waiting = Box::pin(actor.send(message(1, &dropped)));
    pending(waiting.as_mut()).await;
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert_eq!(
        timeout(HUNG, tasks.join_until(cancellation, None, Ok(())))
            .await
            .unwrap(),
        Err(ScopeError::Cancelled)
    );
    assert!(actor.is_closed());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    let rejected = timeout(HUNG, waiting).await.unwrap().unwrap_err().0;
    assert_eq!(rejected.id, 1);
}

on_both_runtimes! {
    async fn try_send_reports_full_and_closed_and_preserves_order() {
        let started = Channel::bounded(4);
        let release = Channel::bounded(4);
        let completed = Actor::new(Vec::new());
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone(), completed.clone()),
            1,
            |state, id: usize| {
                Box::pin(async move {
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    state.2.with(|log| log.push(id));
                    Ok(())
                })
            },
        );

        assert_eq!(actor.try_send(0), Ok(()));
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));
        // Capacity 1 holds one queued message while 0 is in flight.
        assert_eq!(actor.try_send(1), Ok(()));
        assert_eq!(actor.try_send(2), Err(TrySendError::Full(2)));

        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));
        assert_eq!(actor.try_send(2), Ok(()));

        assert!(actor.close());
        assert_eq!(actor.try_send(3), Err(TrySendError::Closed(3)));

        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(2));
        release.send(()).await.unwrap();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        assert_eq!(completed.snapshot(), vec![0, 1, 2]);
    }

    async fn try_send_reports_closed_after_handler_failure() {
        let failed = Channel::bounded(1);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(&mut tasks, failed.clone(), 1, |failed, (): ()| {
            Box::pin(async move {
                failed.send(()).await.unwrap();
                Err("boom")
            })
        });
        assert_eq!(actor.try_send(()), Ok(()));
        timeout(HUNG, failed.receive()).await.unwrap().unwrap();
        assert_eq!(timeout(HUNG, tasks.join(Ok(()))).await.unwrap(), Err("boom"));
        assert_eq!(actor.try_send(()), Err(TrySendError::Closed(())));
    }

    async fn send_until_waits_for_room_and_handles_cancellation_and_close() {
        let started = Channel::bounded(4);
        let release = Channel::bounded(4);
        let completed = Actor::new(Vec::new());
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone(), completed.clone()),
            1,
            |state, id: usize| {
                Box::pin(async move {
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    state.2.with(|log| log.push(id));
                    Ok(())
                })
            },
        );

        let open = Cancellation::new();
        assert_eq!(actor.send_until(&open, None, 0).await, Ok(()));
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));
        assert_eq!(actor.send_until(&open, None, 1).await, Ok(()));

        // Cancel while waiting for room: returns message and never delivers it.
        let cancel = Cancellation::new();
        let mut cancelled_send = Box::pin(actor.send_until(&cancel, None, 99));
        pending(cancelled_send.as_mut()).await;
        cancel.cancel();
        assert_eq!(
            timeout(HUNG, cancelled_send).await.unwrap(),
            Err(SendError::Cancelled(99))
        );

        // Waiting send succeeds once the worker advances.
        let mut waiting = Box::pin(actor.send_until(&open, None, 2));
        pending(waiting.as_mut()).await;
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, waiting).await.unwrap(), Ok(()));
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));

        // Close wakes a waiting send_until with SendError::Closed(message).
        let mut closed_send = Box::pin(actor.send_until(&open, None, 3));
        pending(closed_send.as_mut()).await;
        actor.close();
        assert_eq!(
            timeout(HUNG, closed_send).await.unwrap(),
            Err(SendError::Closed(3))
        );
        assert_eq!(
            actor.send_until(&open, None, 4).await,
            Err(SendError::Closed(4))
        );

        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(2));
        release.send(()).await.unwrap();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        assert_eq!(completed.snapshot(), vec![0, 1, 2]);
    }

    async fn send_until_prefers_shutdown_to_room_and_closure() {
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(&mut tasks, (), 2, |(), _: usize| Box::pin(async { Ok(()) }));

        let cancelled = Cancellation::new();
        cancelled.cancel();
        assert_eq!(
            actor.send_until(&cancelled, past(), 1).await,
            Err(SendError::Cancelled(1))
        );
        assert_eq!(
            actor.send_until(&Cancellation::new(), past(), 2).await,
            Err(SendError::DeadlineExceeded(2))
        );

        actor.close();
        assert_eq!(
            actor.send_until(&cancelled, past(), 3).await,
            Err(SendError::Cancelled(3))
        );
        assert_eq!(
            actor.send_until(&Cancellation::new(), past(), 4).await,
            Err(SendError::DeadlineExceeded(4))
        );
        assert_eq!(
            actor.send_until(&Cancellation::new(), None, 5).await,
            Err(SendError::Closed(5))
        );
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
    }

    async fn send_until_wakes_with_closed_on_handler_failure() {
        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone()),
            1,
            |state, id: usize| {
                Box::pin(async move {
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    Err("handler failed")
                })
            },
        );
        actor.send(0).await.unwrap();
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));
        actor.send(1).await.unwrap();
        let open = Cancellation::new();
        let mut waiting = Box::pin(actor.send_until(&open, None, 2));
        pending(waiting.as_mut()).await;
        release.send(()).await.unwrap();
        assert_eq!(
            timeout(HUNG, waiting).await.unwrap(),
            Err(SendError::Closed(2))
        );
        assert_eq!(
            timeout(HUNG, tasks.join(Ok(()))).await.unwrap(),
            Err("handler failed")
        );
    }

    async fn call_handles_concurrent_requests_borrowing_and_close_drain() {
        enum Command {
            Append { text: String, reply: Reply<usize> },
            Snapshot(Reply<Vec<String>>),
            DropReply(Reply<usize>),
        }

        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (Vec::<String>::new(), started.clone(), release.clone()),
            4,
            |state, cmd: Command| {
                Box::pin(async move {
                    match cmd {
                        Command::Append { text, reply } => {
                            if text == "gated" {
                                state.1.send(()).await.unwrap();
                                state.2.receive().await.unwrap();
                            }
                            state.0.push(text);
                            reply.send(state.0.len()).ok();
                        }
                        Command::Snapshot(reply) => {
                            reply.send(state.0.clone()).ok();
                        }
                        Command::DropReply(_reply) => {}
                    }
                    Ok(())
                })
            },
        );

        // Borrowed local data inside closure.
        let prefix = String::from("item");
        let len = actor
            .call(|reply| Command::Append {
                text: format!("{prefix}-0"),
                reply,
            })
            .await;
        assert_eq!(len, Some(1));

        // Dropping the reply without sending yields None and keeps the worker alive.
        assert_eq!(actor.call(Command::DropReply).await, None);

        // Concurrent spawned callers verify the future is Send.
        let mut handles = Vec::new();
        for i in 1..=4 {
            let actor = actor.clone();
            handles.push(tasks.spawn(async move {
                actor
                    .call(|reply| Command::Append {
                        text: format!("item-{i}"),
                        reply,
                    })
                    .await
                    .ok_or("closed")
            }));
        }
        let mut lengths = Vec::new();
        for handle in handles {
            lengths.push(timeout(HUNG, tasks.value(handle)).await.unwrap().unwrap());
        }
        lengths.sort_unstable();
        assert_eq!(lengths, vec![2, 3, 4, 5]);

        // Closing after a call is accepted still lets the worker drain and reply.
        let caller = actor.clone();
        let gated = tasks.spawn(async move {
            caller
                .call(|reply| Command::Append {
                    text: String::from("gated"),
                    reply,
                })
                .await
                .ok_or("closed")
        });
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        let trailing_caller = actor.clone();
        let trailing = tasks.spawn(async move {
            trailing_caller
                .call(Command::Snapshot)
                .await
                .ok_or("closed")
        });
        // Wait until trailing is accepted (or yield until queued), then close after gated is released.
        // Actually send trailing directly via a spawned task only after we know it's accepted:
        // Use a dedicated test below for exact close-after-accept; here let's close after snapshot.
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, tasks.value(gated)).await.unwrap(), Ok(6));
        let snapshot = timeout(HUNG, tasks.value(trailing)).await.unwrap().unwrap();
        assert_eq!(snapshot.len(), 6);

        actor.close();
        // Closed actor does not invoke the request builder.
        let built = Arc::new(AtomicUsize::new(0));
        let built_flag = built.clone();
        assert_eq!(
            actor
                .call(move |reply| {
                    built_flag.fetch_add(1, Ordering::SeqCst);
                    Command::Snapshot(reply)
                })
                .await,
            None
        );
        assert_eq!(built.load(Ordering::SeqCst), 0);
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
    }

    async fn call_and_call_until_receive_reply_when_closed_after_accept() {
        let started = Channel::bounded(2);
        let release = Channel::bounded(2);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone()),
            2,
            |state, (id, reply): (usize, Reply<usize>)| {
                Box::pin(async move {
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    reply.send(id * 10).ok();
                    Ok(())
                })
            },
        );

        let open = Cancellation::new();
        let mut first = Box::pin(actor.call(|reply| (1, reply)));
        pending(first.as_mut()).await;
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));

        let mut second = Box::pin(actor.call_until(&open, None, |reply| (2, reply)));
        pending(second.as_mut()).await;

        // Close while first is in flight and second is queued: both must drain and reply.
        assert!(actor.close());
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, first).await.unwrap(), Some(10));
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(2));
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, second).await.unwrap(), Ok(20));

        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
    }

    async fn call_and_call_until_return_closed_on_handler_failure_and_cancellation() {
        // Handler fails on in-flight call while another call is queued and a third waits for room.
        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone()),
            1,
            |state, (id, reply): (usize, Reply<usize>)| {
                Box::pin(async move {
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    drop(reply);
                    Err("worker failed")
                })
            },
        );
        let open = Cancellation::new();
        let mut in_flight = Box::pin(actor.call(|reply| (0, reply)));
        pending(in_flight.as_mut()).await;
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));

        let mut queued = Box::pin(actor.call_until(&open, None, |reply| (1, reply)));
        pending(queued.as_mut()).await;

        let mut waiting_room = Box::pin(actor.call_until(&open, None, |reply| (2, reply)));
        pending(waiting_room.as_mut()).await;

        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, in_flight).await.unwrap(), None);
        assert_eq!(timeout(HUNG, queued).await.unwrap(), Err(ReceiveError::Closed));
        assert_eq!(
            timeout(HUNG, waiting_room).await.unwrap(),
            Err(ReceiveError::Closed)
        );
        assert_eq!(
            timeout(HUNG, tasks.join(Ok(()))).await.unwrap(),
            Err("worker failed")
        );

        // Scope cancellation wakes both in-flight and queued calls with None / ReceiveError::Closed.
        let started = Channel::bounded(1);
        let scope_cancel = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            started.clone(),
            1,
            |started, (_id, reply): (usize, Reply<usize>)| {
                Box::pin(async move {
                    started.send(()).await.unwrap();
                    std::future::pending::<()>().await;
                    drop(reply);
                    Ok(())
                })
            },
        );
        let mut in_flight = Box::pin(actor.call(|reply| (0, reply)));
        pending(in_flight.as_mut()).await;
        timeout(HUNG, started.receive()).await.unwrap().unwrap();
        let mut queued = Box::pin(actor.call_until(&open, None, |reply| (1, reply)));
        pending(queued.as_mut()).await;

        scope_cancel.cancel();
        assert_eq!(
            timeout(HUNG, tasks.join_until(scope_cancel, None, Ok(())))
                .await
                .unwrap(),
            Err(ScopeError::Cancelled)
        );
        assert_eq!(timeout(HUNG, in_flight).await.unwrap(), None);
        assert_eq!(timeout(HUNG, queued).await.unwrap(), Err(ReceiveError::Closed));
    }

    #[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
    async fn call_returns_none_when_handler_panics() {
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(&mut tasks, (), 1, |(), reply: Reply<usize>| {
            Box::pin(async move {
                let _reply = reply;
                panic!("call panic");
            })
        });
        assert_eq!(timeout(HUNG, actor.call(|reply| reply)).await.unwrap(), None);
        let panic = timeout(
            HUNG,
            std::panic::AssertUnwindSafe(tasks.join(Ok(()))).catch_unwind(),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"call panic"));
    }

    async fn call_until_precedence_and_cancellation_before_accept() {
        let started = Channel::bounded(1);
        let release = Channel::bounded(1);
        let seen = Actor::new(Vec::new());
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), release.clone(), seen.clone()),
            1,
            |state, (id, reply): (usize, Reply<usize>)| {
                Box::pin(async move {
                    state.2.with(|log| log.push(id));
                    state.0.send(id).await.unwrap();
                    state.1.receive().await.unwrap();
                    reply.send(id).ok();
                    Ok(())
                })
            },
        );

        // Pre-cancelled and past-deadline win without invoking the request builder.
        let built = Arc::new(AtomicUsize::new(0));
        let cancelled = Cancellation::new();
        cancelled.cancel();
        let count = built.clone();
        assert_eq!(
            actor
                .call_until(&cancelled, past(), move |reply| {
                    count.fetch_add(1, Ordering::SeqCst);
                    (100, reply)
                })
                .await,
            Err(ReceiveError::Cancelled)
        );
        let count = built.clone();
        assert_eq!(
            actor
                .call_until(&Cancellation::new(), past(), move |reply| {
                    count.fetch_add(1, Ordering::SeqCst);
                    (101, reply)
                })
                .await,
            Err(ReceiveError::DeadlineExceeded)
        );
        assert_eq!(built.load(Ordering::SeqCst), 0);

        // Fill in-flight (0) + queue (1), then cancel a call_until waiting for room.
        let open = Cancellation::new();
        let mut first = Box::pin(actor.call(|reply| (0, reply)));
        pending(first.as_mut()).await;
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(0));
        let mut second = Box::pin(actor.call(|reply| (1, reply)));
        pending(second.as_mut()).await;

        let cancel_waiting = Cancellation::new();
        let mut blocked = Box::pin(actor.call_until(&cancel_waiting, None, |reply| (2, reply)));
        pending(blocked.as_mut()).await;
        cancel_waiting.cancel();
        assert_eq!(
            timeout(HUNG, blocked).await.unwrap(),
            Err(ReceiveError::Cancelled)
        );

        // Message 2 must never be delivered to the handler.
        actor.close();
        assert_eq!(
            actor
                .call_until(&cancelled, past(), |reply| (3, reply))
                .await,
            Err(ReceiveError::Cancelled)
        );
        assert_eq!(
            actor
                .call_until(&Cancellation::new(), past(), |reply| (4, reply))
                .await,
            Err(ReceiveError::DeadlineExceeded)
        );
        assert_eq!(
            actor.call_until(&open, None, |reply| (5, reply)).await,
            Err(ReceiveError::Closed)
        );

        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, first).await.unwrap(), Some(0));
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));
        release.send(()).await.unwrap();
        assert_eq!(timeout(HUNG, second).await.unwrap(), Some(1));
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
        assert_eq!(seen.snapshot(), vec![0, 1]);
    }

    async fn call_until_caller_cancel_after_accept_closes_handler_reply_and_keeps_worker_alive() {
        #[derive(Debug, PartialEq, Eq)]
        enum HandlerSaw {
            CallerGaveUp(Closed<u32>),
            Sent(u32),
        }

        let started = Channel::bounded(2);
        let observed = Channel::bounded(2);
        let mut tasks = Tasks::<&str>::new();
        let actor = AsyncActor::new(
            &mut tasks,
            (started.clone(), observed.clone()),
            2,
            |state, (id, wait_for_give_up, reply): (u32, bool, Reply<u32>)| {
                Box::pin(async move {
                    if wait_for_give_up {
                        state.0.send(id).await.unwrap();
                        // Handler races its slow work against reply.closed().
                        let abandoned = race((
                            async {
                                reply.closed().await;
                                true
                            },
                            async {
                                std::future::pending::<()>().await;
                                false
                            },
                        ))
                        .await;
                        assert!(abandoned);
                        assert!(reply.is_closed());
                        let err = reply.send(id * 10).unwrap_err();
                        state.1.send(HandlerSaw::CallerGaveUp(err)).await.unwrap();
                    } else {
                        assert!(!reply.is_closed());
                        reply.send(id * 10).unwrap();
                        state.1.send(HandlerSaw::Sent(id * 10)).await.unwrap();
                    }
                    Ok(())
                })
            },
        );

        // 1. Caller cancels via call_until after message is accepted.
        let cancel = Cancellation::new();
        let mut call = Box::pin(actor.call_until(&cancel, None, |reply| (1, true, reply)));
        pending(call.as_mut()).await;
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(1));
        cancel.cancel();
        assert_eq!(timeout(HUNG, call).await.unwrap(), Err(ReceiveError::Cancelled));
        assert_eq!(
            timeout(HUNG, observed.receive()).await.unwrap(),
            Some(HandlerSaw::CallerGaveUp(Closed(10)))
        );

        // 2. Dropping a call future after acceptance also closes the handler's Reply.
        let mut dropped_call = Box::pin(actor.call(|reply| (2, true, reply)));
        pending(dropped_call.as_mut()).await;
        assert_eq!(timeout(HUNG, started.receive()).await.unwrap(), Some(2));
        drop(dropped_call);
        assert_eq!(
            timeout(HUNG, observed.receive()).await.unwrap(),
            Some(HandlerSaw::CallerGaveUp(Closed(20)))
        );

        // 3. The actor worker stayed alive and handles subsequent calls normally.
        assert_eq!(
            timeout(HUNG, actor.call(|reply| (3, false, reply)))
                .await
                .unwrap(),
            Some(30)
        );
        assert_eq!(
            timeout(HUNG, observed.receive()).await.unwrap(),
            Some(HandlerSaw::Sent(30))
        );

        actor.close();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
    }
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn send_until_and_call_until_deadlines_before_and_after_accept() {
    let started = Channel::bounded(4);
    let release = Channel::bounded(4);
    let saw_closed = Actor::new(Vec::new());
    let mut tasks = Tasks::<&str>::new();
    let actor = AsyncActor::new(
        &mut tasks,
        (started.clone(), release.clone(), saw_closed.clone()),
        1,
        |state, (id, reply): (usize, Option<Reply<usize>>)| {
            Box::pin(async move {
                state.0.send(id).await.unwrap();
                match reply {
                    None => {
                        state.1.receive().await.unwrap();
                    }
                    Some(reply) => {
                        if reply.is_closed() {
                            // Caller timed out while this request was still queued.
                            let err = reply.send(id).unwrap_err();
                            state.2.with(|log| log.push((id, "queued", err.0)));
                        } else {
                            // Wait for either release or caller giving up.
                            let gave_up = race((
                                async {
                                    reply.closed().await;
                                    true
                                },
                                async {
                                    state.1.receive().await.unwrap();
                                    false
                                },
                            ))
                            .await;
                            if gave_up {
                                let err = reply.send(id).unwrap_err();
                                state.2.with(|log| log.push((id, "in_flight", err.0)));
                            } else {
                                reply.send(id * 10).unwrap();
                            }
                        }
                    }
                }
                Ok(())
            })
        },
    );

    let open = Cancellation::new();
    // 1. Fill in-flight (0) + queue (1), then hit deadline in send_until and call_until waiting for room.
    actor.send((0, None)).await.unwrap();
    assert_eq!(started.receive().await, Some(0));
    actor.send((1, None)).await.unwrap();

    let deadline = time::now() + Duration::from_secs(5);
    assert!(matches!(
        actor.send_until(&open, Some(deadline), (99, None)).await,
        Err(SendError::DeadlineExceeded((99, None)))
    ));
    assert!(time::now() >= deadline);

    let deadline = time::now() + Duration::from_secs(5);
    assert_eq!(
        actor
            .call_until(&open, Some(deadline), |reply| (98, Some(reply)))
            .await,
        Err(ReceiveError::DeadlineExceeded)
    );
    assert!(time::now() >= deadline);

    // Drain 0 and 1.
    release.send(()).await.unwrap();
    assert_eq!(started.receive().await, Some(1));
    release.send(()).await.unwrap();

    // 2. Message 2 accepted and in-flight; message 3 accepted and queued behind 2.
    // Both time out before release: handler sees in-flight closed on 2 and already-closed on 3.
    let deadline_2 = time::now() + Duration::from_secs(3);
    let deadline_3 = time::now() + Duration::from_secs(2);
    let mut call_2 = Box::pin(actor.call_until(&open, Some(deadline_2), |reply| (2, Some(reply))));
    pending(call_2.as_mut()).await;
    assert_eq!(started.receive().await, Some(2));

    let mut call_3 = Box::pin(actor.call_until(&open, Some(deadline_3), |reply| (3, Some(reply))));
    pending(call_3.as_mut()).await;

    // Advance 2s: call_3 (queued) times out first while 2 is still in flight.
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(call_3.await, Err(ReceiveError::DeadlineExceeded));
    pending(call_2.as_mut()).await;

    // Advance 1 more second: call_2 (in flight) times out; handler wakes via reply.closed(),
    // finishes 2, then dequeues 3 and sees reply.is_closed() == true immediately.
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(call_2.await, Err(ReceiveError::DeadlineExceeded));
    assert_eq!(started.receive().await, Some(3));

    // 3. Subsequent call succeeds cleanly.
    let mut call_4 = Box::pin(actor.call_until(
        &open,
        Some(time::now() + Duration::from_secs(10)),
        |reply| (4, Some(reply)),
    ));
    pending(call_4.as_mut()).await;
    assert_eq!(started.receive().await, Some(4));
    release.send(()).await.unwrap();
    assert_eq!(call_4.await, Ok(40));

    actor.close();
    tasks.join(Ok(())).await.unwrap();
    assert_eq!(
        saw_closed.snapshot(),
        vec![(2, "in_flight", 2), (3, "queued", 3)]
    );
}

#[tokio::test]
async fn call_until_prefers_shutdown_even_after_handler_already_replied() {
    let replied = Channel::bounded(1);
    let mut tasks = Tasks::<&str>::new();
    let actor = AsyncActor::new(
        &mut tasks,
        replied.clone(),
        1,
        |replied, reply: Reply<u32>| {
            Box::pin(async move {
                // Caller has not given up yet, so send succeeds.
                reply.send(77).unwrap();
                replied.send(()).await.unwrap();
                Ok(())
            })
        },
    );

    let cancel = Cancellation::new();
    let mut call = Box::pin(actor.call_until(&cancel, None, |reply| reply));
    pending(call.as_mut()).await;
    timeout(HUNG, replied.receive()).await.unwrap().unwrap();
    // Cancel after the reply was sent, before call_until is polled again.
    cancel.cancel();
    assert_eq!(
        timeout(HUNG, call).await.unwrap(),
        Err(ReceiveError::Cancelled)
    );
    actor.close();
    timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
}
