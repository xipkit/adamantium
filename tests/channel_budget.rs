#![cfg(feature = "tokio")]

use adamantium::{Cancellation, Channel, Fanout, Reply, SendError, Topic, TryReceiveError, Watch};
use futures_util::{FutureExt, StreamExt};
use std::{
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

async fn ready_loop() {
    let (tx, rx) = Channel::bounded(1).split();
    for _ in 0..256 {
        assert_eq!(tx.send(()).await, Ok(()));
        assert_eq!(rx.receive().await, Some(()));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ready_loop_gives_a_runnable_peer_a_turn() {
    let ran = Arc::new(AtomicBool::new(false));
    let peer = ran.clone();
    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the Tokio scheduler directly"
    )]
    let task = tokio::spawn(async move { peer.store(true, Ordering::Relaxed) });
    ready_loop().await;
    assert!(ran.load(Ordering::Relaxed));
    task.await.unwrap();
}

async fn exhaust_budget() -> usize {
    let mut consumed = 0;
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::coop::consume_budget().await;
        consumed += 1;
    }
    consumed
}

#[tokio::test]
async fn exhausted_budget_defers_delivery_and_leaves_values_unseen() {
    let (sender, mut receiver) = Channel::bounded(1).split();
    sender.try_send(1).unwrap();
    let (reply, answer) = Reply::pair();
    reply.send(2).unwrap();
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    watch.send(3).unwrap();
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    fanout.send(4).unwrap();
    let mut topic = Topic::new(1);
    let empty_topic = topic.send(0);
    exhaust_budget().await;

    assert!(receiver.receive().now_or_never().is_none());
    assert!(receiver.next().now_or_never().is_none());
    assert_eq!(receiver.try_receive(), Ok(1));
    assert!(sender.send(9).now_or_never().is_none());
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Empty));
    assert!(answer.receive().now_or_never().is_none());
    assert_eq!(answer.try_receive(), Ok(2));
    assert!(watcher.changed().now_or_never().is_none());
    assert!(watcher.receive().now_or_never().is_none());
    assert!(watcher.wait_for(|_| true).now_or_never().is_none());
    assert_eq!(watcher.try_receive(), Ok(3));
    assert!(feed.receive().now_or_never().is_none());
    assert_eq!(feed.try_receive(), Ok(Some(4)));
    assert!(empty_topic.now_or_never().is_none());

    let first = topic.subscribe();
    let second = topic.subscribe();
    let cancel = Cancellation::new();
    let mut send = pin!(topic.send_until(&cancel, None, 5));
    assert!(send.as_mut().now_or_never().is_none());
    cancel.cancel();
    assert_eq!(send.await, Err(SendError::Cancelled(5)));
    assert_eq!(first.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(second.try_receive(), Err(TryReceiveError::Empty));
}

#[tokio::test]
async fn blocked_polls_refund_the_budget_and_topic_charges_once() {
    let mut topic = Topic::new(1);
    let feeds: Vec<_> = (0..256).map(|_| topic.subscribe()).collect();
    let units = exhaust_budget().await;
    tokio::task::yield_now().await;
    for _ in 1..units {
        tokio::task::coop::consume_budget().await;
    }
    for feed in &feeds {
        assert!(feed.receive().now_or_never().is_none());
    }
    assert_eq!(topic.send(7).now_or_never(), Some(feeds.len()));
    assert!(!tokio::task::coop::has_budget_remaining());
    for feed in feeds {
        assert_eq!(feed.try_receive(), Ok(7));
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn model_replays_pending_and_resumes_ready_loops() {
    use adamantium::{Tasks, testing};

    async fn observe() {
        let mut work = pin!(ready_loop());
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            let result = work.as_mut().poll(cx);
            yielded |= result.is_pending();
            result
        })
        .await;
        assert!(yielded, "the ready loop finished in one poll");
    }

    async fn scenario() {
        observe().await;
        testing::parallel([observe(), observe()]).await;
        Tasks::<()>::run(async |tasks| {
            let worker = tasks.spawn(async {
                observe().await;
                Ok(())
            });
            tasks.value(worker).await
        })
        .await
        .unwrap();
    }

    let config = testing::Config {
        iterations: 2,
        max_steps: 100_000,
        ..Default::default()
    };
    let schedule = testing::check(config, scenario).unwrap();
    assert_eq!(testing::replay(&schedule, scenario).unwrap(), schedule);
}
