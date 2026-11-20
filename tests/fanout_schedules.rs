#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "schedule observations and deliberate execution-boundary misuse"
)]

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use adamantium::{
    Actor, Closed, Ended, Fanout, Feed,
    testing::{self, Config, FailureKind},
};

type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

fn config() -> Config {
    Config {
        seed: 201,
        iterations: 500,
        max_steps: 20_000,
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

async fn read_all(mut feed: Feed<(u32, u32)>) -> (Vec<(u32, u32)>, Ended) {
    let mut events = Vec::new();
    loop {
        match feed.receive().await {
            Ok(event) => events.push(event),
            Err(end) => return (events, end),
        }
    }
}

#[test]
fn concurrent_publishers_readers_and_a_dropped_feed_replay() {
    check(|| {
        Box::pin(async {
            let fanout = Fanout::new(4);
            let first = fanout.subscribe();
            let second = fanout.subscribe();
            let unread = fanout.subscribe();
            let dropped = fanout.subscribe();
            let orders = Actor::new(Vec::new());
            let publishers: Vec<Worker> = (0..2)
                .map(|publisher| {
                    let fanout = fanout.clone();
                    Box::pin(async move {
                        for n in 0..2 {
                            fanout.send((publisher, n)).unwrap();
                        }
                    }) as Worker
                })
                .collect();
            let mut workers: Vec<Worker> = vec![Box::pin(async move {
                testing::parallel(publishers).await;
                assert!(fanout.close());
            })];
            for feed in [first, second] {
                let orders = orders.clone();
                workers.push(Box::pin(async move {
                    let (events, end) = read_all(feed).await;
                    assert_eq!(end, Ended::Closed);
                    orders.with(|orders| orders.push(events));
                }));
            }
            workers.push(Box::pin(async move { drop(dropped) }));
            testing::parallel(workers).await;

            let orders = orders.into_inner().ok().unwrap();
            assert_eq!(orders.len(), 2);
            assert_eq!(orders[0], orders[1], "every feed sees one order");
            for publisher in 0..2 {
                let own: Vec<u32> = orders[0]
                    .iter()
                    .filter(|(from, _)| *from == publisher)
                    .map(|(_, n)| *n)
                    .collect();
                assert_eq!(own, [0, 1], "each publisher's sends in order");
            }
            let (events, end) = read_all(unread).await;
            assert_eq!(events, orders[0]);
            assert_eq!(end, Ended::Closed);
        })
    });
}

#[test]
fn an_overflow_racing_a_reader_cuts_off_only_a_full_feed_replay() {
    check(|| {
        Box::pin(async {
            let fanout = Fanout::new(1);
            let mut feed = fanout.subscribe();
            fanout.send(0).unwrap();
            let publisher = fanout.clone();
            let outcome = Actor::new(None);
            let reported = outcome.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    let sent = publisher.send(1).unwrap();
                    reported.with(|outcome| *outcome = Some(sent.disconnected));
                    publisher.close();
                }),
                Box::pin(async move {
                    let mut events = Vec::new();
                    let end = loop {
                        match feed.receive().await {
                            Ok(event) => events.push(event),
                            Err(end) => break end,
                        }
                    };
                    match end {
                        Ended::Closed => assert_eq!(events, [0, 1]),
                        Ended::Overflowed => assert_eq!(events, [0]),
                    }
                }),
            ];
            testing::parallel(workers).await;
            assert!(matches!(outcome.snapshot(), Some(0 | 1)));
        })
    });
}

#[test]
fn close_racing_send_returns_exactly_the_unsent_event_replay() {
    check(|| {
        Box::pin(async {
            let fanout = Fanout::new(2);
            let mut feed = fanout.subscribe();
            let closer = fanout.clone();
            let sent = Actor::new(None);
            let reported = sent.clone();
            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    let result = fanout.send(7).map(|sent| sent.delivered);
                    reported.with(|sent| *sent = Some(result));
                }),
                Box::pin(async move {
                    assert!(closer.close());
                }),
            ];
            testing::parallel(workers).await;
            let expected = match sent.snapshot() {
                Some(Ok(1)) => Ok(Some(7)),
                Some(Err(Closed(7))) => Err(Ended::Closed),
                other => panic!("unexpected send result {other:?}"),
            };
            assert_eq!(feed.try_receive(), expected);
            assert_eq!(feed.try_receive(), Err(Ended::Closed));
        })
    });
}

#[test]
fn fanouts_support_the_model_and_reject_foreign_handles() {
    fn carried_in<V: Send + 'static>(value: V, body: fn(V)) -> FailureKind {
        let value = Mutex::new(Some(value));
        testing::check(
            Config {
                iterations: 1,
                ..config()
            },
            move || {
                let value = value.lock().unwrap().take().unwrap();
                async move { body(value) }
            },
        )
        .unwrap_err()
        .kind()
    }

    let fanout = Fanout::new(1);
    let feed = fanout.subscribe();
    assert_eq!(
        carried_in(fanout.clone(), |fanout| assert!(fanout.send(1).is_ok())),
        FailureKind::Panic
    );
    assert_eq!(
        carried_in(fanout.clone(), |fanout| drop(fanout.subscribe())),
        FailureKind::Panic
    );
    let receiving = Arc::new(Mutex::new(Some(feed)));
    let carried = Arc::clone(&receiving);
    let failure = testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let mut feed = carried.lock().unwrap().take().unwrap();
            async move {
                assert_eq!(feed.try_receive(), Ok(None));
                drop(feed);
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    assert_eq!(
        carried_in(fanout.clone(), |fanout| {
            let _held = fanout;
            std::panic::panic_any("unwinding");
        }),
        FailureKind::Panic
    );
    assert_eq!(carried_in(fanout.clone(), drop), FailureKind::Panic);
    assert!(
        !fanout.is_closed(),
        "the rejected drop left the count alone"
    );
    assert!(receiving.lock().unwrap().is_none());
    assert_eq!(fanout.subscriber_count(), 0);
}
