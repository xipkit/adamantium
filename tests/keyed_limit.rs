use adamantium::{Actor, Channel, KeyedLimit, Limit};
use std::{
    future::{Future, pending, ready},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn stale_session_requests_share_one_refresh_inside_the_connection_limit() {
    let connections = Limit::new(3);
    let refreshes = KeyedLimit::new();
    let sessions = Actor::new(["stale-a", "stale-b"]);
    let spent = Actor::new(Vec::new());
    let release_a = Channel::bounded(1);

    let request = async |id: usize| {
        connections
            .run(|| async {
                let stale = sessions.read(|rows| rows[id]);
                assert!(stale.starts_with("stale"));
                refreshes
                    .run(id, || async {
                        let current = sessions.read(|rows| rows[id]);
                        if current != stale {
                            return current;
                        }
                        spent.with(|tokens| {
                            assert!(!tokens.contains(&current), "single-use token spent twice");
                            tokens.push(current);
                        });
                        if id == 0 {
                            release_a.receive().await.unwrap();
                        }
                        let rotated = ["rotated-a", "rotated-b"][id];
                        sessions.with(|rows| rows[id] = rotated);
                        rotated
                    })
                    .await
            })
            .await
    };

    let mut first = pin!(request(0));
    let mut second = pin!(request(0));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    assert_eq!(connections.available(), 1);
    assert_eq!(poll_once(pin!(request(1))), Poll::Ready("rotated-b"));
    assert_eq!(sessions.read(|rows| rows[0]), "stale-a");
    release_a.try_send(()).unwrap();
    assert_eq!(poll_once(first.as_mut()), Poll::Ready("rotated-a"));
    assert_eq!(poll_once(second.as_mut()), Poll::Ready("rotated-a"));
    assert_eq!(spent.read(Clone::clone), ["stale-a", "stale-b"]);
    assert_eq!(connections.available(), 3);
}

#[derive(Default)]
struct Woken(AtomicBool);

impl Woken {
    fn received(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn dropping_a_notified_waiter_hands_off_the_wake_and_evicts_the_idle_key() {
    let table = KeyedLimit::new();
    let clone = table.clone();
    let key = Arc::new(7);
    let mut holder = Box::pin(table.run(key.clone(), pending::<()>));
    assert!(poll_once(holder.as_mut()).is_pending());
    {
        let mut cancelled = pin!(table.run(key.clone(), || ready(())));
        assert!(poll_once(cancelled.as_mut()).is_pending());
    }
    let [first, second] = [&table, &clone].map(|table| {
        let flag = Arc::new(Woken::default());
        let waker = Waker::from(flag.clone());
        let mut waiter = Box::pin(table.run(key.clone(), pending::<()>));
        assert!(
            waiter
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        (waiter, flag)
    });

    drop(holder);
    let ((notified, original), (mut survivor, forwarded)) = if first.1.received() {
        (first, second)
    } else {
        (second, first)
    };
    assert!(original.received());
    assert!(!forwarded.received());
    drop(notified);
    assert!(forwarded.received());
    assert!(poll_once(survivor.as_mut()).is_pending());

    let mut late = pin!(table.run(key.clone(), || ready(42)));
    assert!(poll_once(late.as_mut()).is_pending());
    drop(survivor);
    assert_eq!(poll_once(late.as_mut()), Poll::Ready(42));
    assert_eq!(Arc::strong_count(&key), 1, "idle key retained");
}

#[test]
fn dropping_the_last_waiter_evicts_its_key() {
    let keys = KeyedLimit::new();
    let key = Arc::new(7);
    let mut holder = Box::pin(keys.run(key.clone(), pending::<()>));
    assert!(poll_once(holder.as_mut()).is_pending());
    {
        let mut waiter = pin!(keys.run(key.clone(), || ready(())));
        assert!(poll_once(waiter.as_mut()).is_pending());
        drop(holder);
    }
    assert_eq!(Arc::strong_count(&key), 1);
}

#[cfg(not(target_os = "wasi"))]
mod nesting {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn rejected<F: Future>(future: F) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _result = poll_once(pin!(future));
        }));
        let message = result.as_ref().err().and_then(|panic| {
            panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
        });
        assert!(
            message.is_some_and(|m| m.contains("already holds a place")),
            "{message:?}"
        );
    }

    #[test]
    fn ordinary_limits_reject_both_orders_even_in_the_work_constructor() {
        let a = Limit::new(2);
        let b = Limit::new(2);
        for (outer, inner) in [(&a, &b), (&b, &a)] {
            assert_eq!(
                poll_once(pin!(outer.run(|| {
                    rejected(inner.run(|| ready(())));
                    ready(())
                }))),
                Poll::Ready(())
            );
        }
    }

    #[test]
    fn keyed_work_rejects_all_nested_acquisitions_and_restores_the_outer_scope() {
        let connections = Limit::new(2);
        let keys = KeyedLimit::new();
        let other = KeyedLimit::new();
        let request = connections.run(|| async {
            keys.run("a", || {
                rejected(connections.run(|| ready(())));
                ready(())
            })
            .await;
            keys.run("a", || async {
                rejected(keys.run("a", || ready(())));
                rejected(keys.run("b", || ready(())));
                rejected(other.run("b", || ready(())));
            })
            .await;
            rejected(connections.run(|| ready(())));
        });
        assert_eq!(poll_once(pin!(request)), Poll::Ready(()));
        assert_eq!(
            poll_once(pin!(connections.run(|| ready(1)))),
            Poll::Ready(1)
        );
    }

    #[test]
    fn unwinding_keyed_work_restores_the_connection_scope_and_releases_the_key() {
        let keys = KeyedLimit::new();
        let connections = Limit::new(2);
        let request = connections.run(|| async {
            let failed = keys.run(1, || async { panic!("refresh failed") });
            assert!(catch_unwind(AssertUnwindSafe(|| poll_once(pin!(failed)))).is_err());
            rejected(connections.run(|| ready(())));
            keys.run(1, || ready(42)).await
        });
        assert_eq!(poll_once(pin!(request)), Poll::Ready(42));
    }

    #[test]
    fn a_waiter_first_polled_outside_the_key_cannot_be_resumed_inside_it() {
        let keys = KeyedLimit::new();
        let mut holder = pin!(keys.run("a", pending::<()>));
        assert!(poll_once(holder.as_mut()).is_pending());
        let mut waiter = pin!(keys.run("a", || ready(())));
        assert!(poll_once(waiter.as_mut()).is_pending());
        assert!(
            poll_once(pin!(keys.run("b", || async {
                rejected(waiter.as_mut());
            })))
            .is_ready()
        );
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn run_until_uses_the_same_acquisition_order() {
        use adamantium::Cancellation;
        let stop = Cancellation::new();
        let connections = Limit::new(2);
        let keys = KeyedLimit::new();
        let output = connections
            .run_until(&stop, None, || {
                keys.run_until(1, &stop, None, || async {
                    rejected(connections.run_until(&stop, None, || ready(Ok::<_, ()>(()))));
                    rejected(keys.run_until(2, &stop, None, || ready(Ok::<_, ()>(()))));
                    Ok::<_, ()>(42)
                })
            })
            .await;
        assert_eq!(output, Ok(42));
    }
}

#[cfg(all(feature = "tokio", not(target_os = "wasi")))]
#[tokio::test(start_paused = true)]
async fn run_until_cancels_a_waiter_and_releases_an_expired_holder() {
    use adamantium::{Cancellation, ScopeError, time};
    use std::time::Duration;
    let keys = KeyedLimit::new();
    let holder_stop = Cancellation::new();
    let waiter_stop = Cancellation::new();
    let deadline = time::now() + Duration::from_millis(30);
    let mut holder =
        pin!(keys.run_until(1, &holder_stop, Some(deadline), pending::<Result<(), ()>>));
    assert!(poll_once(holder.as_mut()).is_pending());
    let mut waiter = pin!(keys.run_until(1, &waiter_stop, None, || ready(Ok::<_, ()>(1))));
    assert!(poll_once(waiter.as_mut()).is_pending());
    waiter_stop.cancel();
    assert_eq!(waiter.await, Err(ScopeError::Cancelled));
    assert_eq!(holder.await, Err(ScopeError::DeadlineExceeded));
    assert_eq!(keys.run(1, || ready(2)).await, 2);
}

#[cfg(feature = "deterministic")]
#[test]
fn modeled_arrivals_and_evictions_keep_one_holder_per_key() {
    use adamantium::testing::{self, Config};
    let checked = testing::check(
        Config {
            seed: 199,
            iterations: 300,
            max_steps: 20_000,
            ..Config::default()
        },
        || async {
            let connections = Limit::new(3);
            let keys = KeyedLimit::new();
            let active = Actor::new([false; 2]);
            let workers = (0..5)
                .map(|i| {
                    let (connections, keys, active) =
                        (connections.clone(), keys.clone(), active.clone());
                    async move {
                        let key = i % 2;
                        connections
                            .run(|| {
                                keys.run(key, || async {
                                    active.with(|state| {
                                        assert!(!std::mem::replace(&mut state[key], true))
                                    });
                                    active.with(|state| assert!(std::mem::take(&mut state[key])));
                                })
                            })
                            .await;
                    }
                })
                .collect::<Vec<_>>();
            testing::parallel(workers).await;
        },
    );
    assert!(checked.is_ok(), "{checked:?}");
}
