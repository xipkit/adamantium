#![cfg(feature = "tokio")]

use adamantium::{Cancellation, KeyedLimit, Limit, ScopeError};
use futures_util::FutureExt;
use std::{future::pending, pin::pin};

async fn ready_loop(limit: &Limit) {
    for _ in 0..512 {
        limit.run(|| async {}).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uncontended_acquisitions_give_a_runnable_peer_a_turn() {
    #[expect(clippy::disallowed_methods, reason = "queue a peer directly on Tokio")]
    let peer = tokio::spawn(async {});
    ready_loop(&Limit::new(1)).await;
    assert!(
        peer.is_finished(),
        "the peer was still queued after 512 calls"
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn cancellation_wins_while_acquisition_is_out_of_budget() {
    let limit = Limit::new(1);
    let keys = KeyedLimit::new();
    let cancel = Cancellation::new();
    while tokio::task::coop::has_budget_remaining() {
        tokio::task::coop::consume_budget().await;
    }

    let mut ordinary = pin!(limit.run_until(&cancel, None, || async { Ok::<_, ()>(()) }));
    let mut keyed = pin!(keys.run_until("key", &cancel, None, || async { Ok::<_, ()>(()) }));
    assert!(ordinary.as_mut().now_or_never().is_none());
    assert!(keyed.as_mut().now_or_never().is_none());
    assert_eq!(limit.available(), 1);
    cancel.cancel();
    assert_eq!(ordinary.now_or_never(), Some(Err(ScopeError::Cancelled)));
    assert_eq!(keyed.now_or_never(), Some(Err(ScopeError::Cancelled)));
}

#[tokio::test]
async fn waiting_for_a_place_spends_no_budget() {
    let limit = Limit::new(1);
    let mut holder = Box::pin(limit.run(pending::<()>));
    assert!(holder.as_mut().now_or_never().is_none());
    let mut waiter = pin!(limit.run(|| async {}));
    for _ in 0..512 {
        assert!(waiter.as_mut().now_or_never().is_none());
    }
    drop(holder);
    assert_eq!(waiter.now_or_never(), Some(()));
}

#[cfg(feature = "deterministic")]
#[test]
fn replay_yields_with_the_place_still_free() {
    use adamantium::testing;

    async fn scenario() {
        let limit = Limit::new(1);
        let mut calls = pin!(ready_loop(&limit));
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            let result = calls.as_mut().poll(cx);
            if result.is_pending() {
                assert_eq!(limit.available(), 1);
                yielded = true;
            }
            result
        })
        .await;
        assert!(yielded, "all 512 calls finished in one poll");
    }

    let config = testing::Config {
        iterations: 1,
        ..Default::default()
    };
    let schedule = testing::check(config, scenario).unwrap();
    assert_eq!(testing::replay(&schedule, scenario).unwrap(), schedule);
}
