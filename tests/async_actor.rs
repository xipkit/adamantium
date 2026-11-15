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

use adamantium::{Actor, AsyncActor, Cancellation, Channel, Closed, ScopeError, Tasks};
use futures_util::FutureExt;
use support::timeout;
use tokio::io::AsyncReadExt;

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

// On the current-thread runtime no spawned worker can run before we yield.
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
