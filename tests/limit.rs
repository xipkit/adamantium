use adamantium::{Channel, Limit, Overweight};
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

#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// A waker for one call, which records whether the call was woken.
struct Watched {
    flag: Arc<Flag>,
    waker: Waker,
}

impl Watched {
    fn new() -> Self {
        let flag = Arc::new(Flag::default());
        Self {
            waker: Waker::from(Arc::clone(&flag)),
            flag,
        }
    }

    fn poll<F: Future>(&self, future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(&self.waker))
    }

    // Whether the call was woken since the last check.
    fn woken(&self) -> bool {
        self.flag.0.swap(false, Ordering::SeqCst)
    }
}

#[test]
fn work_runs_at_once_while_a_place_is_free() {
    let limit = Limit::new(2);
    let mut call = pin!(limit.run(|| ready(7)));
    assert_eq!(poll_once(call.as_mut()), Poll::Ready(7));
    assert_eq!(limit.available(), 2);
}

#[test]
fn a_call_over_the_limit_waits_and_does_not_start_its_work() {
    let limit = Limit::new(1);
    let release = Channel::<()>::bounded(1);
    let mut first = pin!(limit.run(|| async { release.receive().await }));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(limit.available(), 0);

    let mut started = false;
    {
        let mut second = pin!(limit.run(|| {
            started = true;
            ready(2)
        }));
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);

        release.try_send(()).unwrap();
        assert_eq!(poll_once(first.as_mut()), Poll::Ready(Some(())));
        assert_eq!(poll_once(second.as_mut()), Poll::Ready(2));
    }
    assert!(started);
    assert_eq!(limit.available(), 1);
}

#[test]
fn clones_share_the_limit() {
    let limit = Limit::new(1);
    let clone = limit.clone();
    let mut first = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    let mut second = pin!(clone.run(|| ready(())));
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    assert_eq!(clone.available(), 0);
}

#[test]
fn dropping_a_running_call_frees_its_place() {
    let limit = Limit::new(1);
    {
        let mut call = pin!(limit.run(pending::<()>));
        assert_eq!(poll_once(call.as_mut()), Poll::Pending);
        assert_eq!(limit.available(), 0);
    }
    assert_eq!(limit.available(), 1);
}

#[test]
fn dropping_a_waiting_call_takes_no_place() {
    let limit = Limit::new(1);
    let mut first = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    {
        let mut waiting = pin!(limit.run(|| ready(())));
        assert_eq!(poll_once(waiting.as_mut()), Poll::Pending);
    }
    assert_eq!(limit.available(), 0);
}

#[test]
fn a_limit_of_zero_is_a_limit_of_one() {
    let limit = Limit::new(0);
    assert_eq!(limit.available(), 1);
    let mut call = pin!(limit.run(|| ready(1)));
    assert_eq!(poll_once(call.as_mut()), Poll::Ready(1));
}

#[test]
#[should_panic(expected = "already holds a place under a limit")]
fn work_cannot_call_the_same_limit() {
    let limit = Limit::new(2);
    let inner = limit.clone();
    let mut call = pin!(limit.run(|| async move { inner.run(|| ready(())).await }));
    assert!(poll_once(call.as_mut()).is_pending());
}

#[test]
#[should_panic(expected = "already holds a place under a limit")]
fn work_cannot_call_another_limit() {
    let outer = Limit::new(1);
    let inner = Limit::new(1);
    let mut call = pin!(outer.run(|| async move { inner.run(|| ready(())).await }));
    assert!(poll_once(call.as_mut()).is_pending());
}

#[test]
fn a_call_made_after_the_work_finishes_is_not_nested() {
    let limit = Limit::new(1);
    let mut first = pin!(limit.run(|| ready(1)));
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(1));
    let mut second = pin!(limit.run(|| ready(2)));
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(2));
}

#[test]
fn a_call_polled_between_the_polls_of_running_work_is_not_nested() {
    let limit = Limit::new(2);
    let mut running = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(running.as_mut()), Poll::Pending);
    let mut other = pin!(limit.run(|| ready(2)));
    assert_eq!(poll_once(other.as_mut()), Poll::Ready(2));
}

#[test]
fn a_limit_of_send_work_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let limit = Limit::new(1);
    let call = limit.run(|| ready(1));
    assert_send(&call);
    assert_send(&limit);
}

#[test]
fn a_waiting_call_is_served_before_calls_that_arrive_later() {
    let limit = Limit::new(1);
    let mut holder = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut first = pin!(limit.run(|| ready("first")));
    let first_waker = Watched::new();
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Pending);
    let mut second = pin!(limit.run(|| ready("second")));
    let second_waker = Watched::new();
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Pending);

    drop(holder);
    assert!(first_waker.woken());
    assert!(!second_waker.woken());
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Pending);
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Ready("first"));
    assert!(second_waker.woken());
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Ready("second"));
}

#[test]
fn a_freed_place_wakes_only_the_oldest_waiting_call_and_serving_it_wakes_the_next() {
    let limit = Limit::new(2);
    let mut holders = [(); 2].map(|()| Box::pin(limit.run(pending::<()>)));
    for holder in &mut holders {
        assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    }
    let release = Channel::<()>::bounded(1);
    let mut first = pin!(limit.run(|| release.receive()));
    let first_waker = Watched::new();
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Pending);
    let mut second = pin!(limit.run(|| ready(2)));
    let second_waker = Watched::new();
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Pending);

    drop(holders);
    assert!(first_waker.woken());
    assert!(!second_waker.woken());
    // The first call takes one place and still runs; the second fits in the
    // other, so serving the first wakes it.
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Pending);
    assert!(second_waker.woken());
    assert_eq!(limit.available(), 1);
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Ready(2));
    release.try_send(()).unwrap();
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Ready(Some(())));
    assert_eq!(limit.available(), 2);
}

#[test]
fn later_small_calls_wait_behind_a_large_call_even_while_places_are_free() {
    let limit = Limit::new(3);
    let mut holder = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut large = pin!(limit.weighted(3).unwrap().run(|| ready("large")));
    let large_waker = Watched::new();
    assert_eq!(large_waker.poll(large.as_mut()), Poll::Pending);

    let mut started = false;
    {
        let mut small = pin!(limit.run(|| {
            started = true;
            ready("small")
        }));
        let small_waker = Watched::new();
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Pending);
        assert_eq!(limit.available(), 2);

        drop(holder);
        assert!(large_waker.woken());
        assert!(!small_waker.woken());
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Pending);
        assert_eq!(large_waker.poll(large.as_mut()), Poll::Ready("large"));
        assert!(small_waker.woken());
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Ready("small"));
    }
    assert!(started);
    assert_eq!(limit.available(), 3);
}

#[test]
fn a_waiting_call_is_not_woken_until_all_its_places_are_free() {
    let limit = Limit::new(3);
    let mut first = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    let mut second = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    let mut large = pin!(limit.weighted(3).unwrap().run(|| ready(3)));
    let large_waker = Watched::new();
    assert_eq!(large_waker.poll(large.as_mut()), Poll::Pending);

    drop(first);
    assert!(!large_waker.woken());
    drop(second);
    assert!(large_waker.woken());
    assert_eq!(large_waker.poll(large.as_mut()), Poll::Ready(3));
}

#[test]
fn mixed_weights_share_the_places() {
    let limit = Limit::new(4);
    let mut three = Box::pin(limit.weighted(3).unwrap().run(pending::<()>));
    assert_eq!(poll_once(three.as_mut()), Poll::Pending);
    assert_eq!(limit.available(), 1);
    assert_eq!(poll_once(pin!(limit.run(|| ready(1)))), Poll::Ready(1));

    let mut two = pin!(limit.weighted(2).unwrap().run(|| ready(2)));
    let two_waker = Watched::new();
    assert_eq!(two_waker.poll(two.as_mut()), Poll::Pending);
    assert_eq!(limit.available(), 1);

    drop(three);
    assert!(two_waker.woken());
    assert_eq!(limit.available(), 4);
    assert_eq!(two_waker.poll(two.as_mut()), Poll::Ready(2));
    assert_eq!(limit.available(), 4);
}

#[test]
fn a_running_weighted_call_frees_all_its_places_when_dropped() {
    let limit = Limit::new(5);
    {
        let mut call = pin!(limit.weighted(4).unwrap().run(pending::<()>));
        assert_eq!(poll_once(call.as_mut()), Poll::Pending);
        assert_eq!(limit.available(), 1);
    }
    assert_eq!(limit.available(), 5);
}

#[test]
fn a_weight_above_the_total_is_rejected_without_waiting() {
    let limit = Limit::new(4);
    let mut holder = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);

    let rejected = limit.weighted(5).map(|weighted| weighted.places());
    assert_eq!(
        rejected,
        Err(Overweight {
            places: 5,
            limit: 4
        })
    );
    assert_eq!(
        Overweight {
            places: 5,
            limit: 4
        }
        .to_string(),
        "a call of 5 places can never run under a limit of 4"
    );
    assert_eq!(limit.weighted(4).map(|weighted| weighted.places()), Ok(4));
    assert_eq!(limit.available(), 3);
}

#[test]
fn a_weight_of_zero_is_a_weight_of_one() {
    let limit = Limit::new(0);
    assert_eq!(limit.weighted(0).map(|weighted| weighted.places()), Ok(1));
    assert_eq!(
        limit.weighted(2).map(|weighted| weighted.places()),
        Err(Overweight {
            places: 2,
            limit: 1
        })
    );
    let mut call = pin!(limit.weighted(0).unwrap().run(pending::<()>));
    assert_eq!(poll_once(call.as_mut()), Poll::Pending);
    assert_eq!(limit.available(), 0);
}

#[test]
fn dropping_the_waiting_front_call_wakes_the_call_behind_it() {
    let limit = Limit::new(3);
    let mut holder = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut large = Box::pin(limit.weighted(3).unwrap().run(|| ready(())));
    assert_eq!(poll_once(large.as_mut()), Poll::Pending);
    let mut small = pin!(limit.weighted(2).unwrap().run(|| ready(2)));
    let small_waker = Watched::new();
    assert_eq!(small_waker.poll(small.as_mut()), Poll::Pending);

    drop(large);
    assert!(small_waker.woken());
    assert_eq!(small_waker.poll(small.as_mut()), Poll::Ready(2));
    assert_eq!(limit.available(), 2);
}

#[test]
fn dropping_a_woken_call_wakes_the_call_behind_it() {
    let limit = Limit::new(1);
    let mut holder = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut first = Box::pin(limit.run(|| ready(1)));
    let first_waker = Watched::new();
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Pending);
    let mut second = pin!(limit.run(|| ready(2)));
    let second_waker = Watched::new();
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Pending);

    drop(holder);
    assert!(first_waker.woken());
    assert!(!second_waker.woken());
    drop(first);
    assert!(second_waker.woken());
    assert_eq!(second_waker.poll(second.as_mut()), Poll::Ready(2));
}

#[test]
fn dropping_a_call_behind_the_front_keeps_the_others_in_order() {
    let limit = Limit::new(1);
    let mut holder = Box::pin(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut first = pin!(limit.run(|| ready(1)));
    let first_waker = Watched::new();
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Pending);
    let mut dropped = Box::pin(limit.run(|| ready(2)));
    assert_eq!(poll_once(dropped.as_mut()), Poll::Pending);
    let mut third = pin!(limit.run(|| ready(3)));
    let third_waker = Watched::new();
    assert_eq!(third_waker.poll(third.as_mut()), Poll::Pending);

    drop(dropped);
    assert!(!first_waker.woken());
    assert!(!third_waker.woken());
    drop(holder);
    assert!(first_waker.woken());
    assert!(!third_waker.woken());
    assert_eq!(first_waker.poll(first.as_mut()), Poll::Ready(1));
    assert!(third_waker.woken());
    assert_eq!(third_waker.poll(third.as_mut()), Poll::Ready(3));
}

#[test]
fn dropping_waiters_in_the_middle_and_at_the_front_while_a_later_call_fits() {
    let limit = Limit::new(4);
    let mut holder = pin!(limit.weighted(2).unwrap().run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    let mut front = Box::pin(limit.weighted(4).unwrap().run(|| ready(4)));
    let front_waker = Watched::new();
    assert_eq!(front_waker.poll(front.as_mut()), Poll::Pending);
    let mut middle = Box::pin(limit.run(|| ready(1)));
    assert_eq!(poll_once(middle.as_mut()), Poll::Pending);
    let mut later = pin!(limit.weighted(2).unwrap().run(|| ready(2)));
    let later_waker = Watched::new();
    assert_eq!(later_waker.poll(later.as_mut()), Poll::Pending);

    drop(middle);
    assert!(!front_waker.woken());
    assert!(!later_waker.woken());
    assert_eq!(later_waker.poll(later.as_mut()), Poll::Pending);
    drop(front);
    assert!(later_waker.woken());
    assert_eq!(later_waker.poll(later.as_mut()), Poll::Ready(2));
    assert_eq!(limit.available(), 2);
}

#[test]
fn a_weighted_call_waiting_or_dropped_takes_no_place() {
    let limit = Limit::new(2);
    let mut holder = pin!(limit.run(pending::<()>));
    assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
    {
        let mut waiting = pin!(limit.weighted(2).unwrap().run(|| ready(())));
        assert_eq!(poll_once(waiting.as_mut()), Poll::Pending);
        assert_eq!(limit.available(), 1);
    }
    assert_eq!(limit.available(), 1);
    assert_eq!(poll_once(pin!(limit.run(|| ready(1)))), Poll::Ready(1));
}

#[test]
#[should_panic(expected = "already holds a place under a limit")]
fn weighted_work_cannot_call_a_limit() {
    let limit = Limit::new(4);
    let inner = limit.clone();
    let mut call = pin!(
        limit
            .weighted(2)
            .unwrap()
            .run(|| async move { inner.run(|| ready(())).await })
    );
    assert!(poll_once(call.as_mut()).is_pending());
}

#[test]
#[should_panic(expected = "already holds a place under a limit")]
fn work_cannot_make_a_weighted_call() {
    let outer = Limit::new(1);
    let inner = Limit::new(4);
    let mut call =
        pin!(outer.run(|| async move { inner.weighted(2).unwrap().run(|| ready(())).await }));
    assert!(poll_once(call.as_mut()).is_pending());
}

#[test]
fn a_weighted_call_of_send_work_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let limit = Limit::new(2);
    let weighted = limit.weighted(2).unwrap();
    let call = weighted.run(|| ready(1));
    assert_send(&call);
    assert_send(&weighted);
}

#[cfg(feature = "tokio")]
mod until {
    use super::*;
    use adamantium::{Cancellation, ScopeError, concurrently, time};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[tokio::test]
    async fn a_request_already_present_takes_no_place_and_starts_no_work() {
        let limit = Limit::new(1);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let mut started = false;
        let result = limit
            .run_until(&cancellation, None, || {
                started = true;
                ready(Ok::<_, ()>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
        assert!(!started);
        assert_eq!(limit.available(), 1);
    }

    #[tokio::test]
    async fn the_deadline_ends_a_wait_for_a_place() {
        let limit = Limit::new(1);
        let mut holder = pin!(limit.run(pending::<()>));
        assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
        let deadline = time::now() + Duration::from_millis(20);
        let result = limit
            .run_until(&Cancellation::new(), Some(deadline), || {
                ready(Ok::<_, ()>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert_eq!(limit.available(), 0);
    }

    #[tokio::test]
    async fn the_deadline_drops_running_work_and_frees_its_place() {
        let limit = Limit::new(1);
        let deadline = time::now() + Duration::from_millis(20);
        let result = limit
            .run_until(&Cancellation::new(), Some(deadline), || {
                pending::<Result<(), ()>>()
            })
            .await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert_eq!(limit.available(), 1);
    }

    #[tokio::test]
    async fn the_error_of_the_work_is_returned_as_failed() {
        let limit = Limit::new(1);
        let result = limit
            .run_until(&Cancellation::new(), None, || ready(Err::<(), _>("no")))
            .await;
        assert_eq!(result, Err(ScopeError::Failed("no")));
    }

    #[tokio::test]
    async fn tasks_that_share_a_limit_never_exceed_it() {
        let limit = Limit::new(3);
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let done = concurrently(0..40, 16, |n| {
            let (limit, running, most) = (limit.clone(), running.clone(), most.clone());
            async move {
                limit
                    .run(|| async {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await;
                Ok::<_, ()>(n)
            }
        })
        .await;
        assert_eq!(done.map(|done| done.len()), Ok(40));
        assert!(most.load(Ordering::SeqCst) <= 3);
        assert_eq!(limit.available(), 3);
    }

    #[tokio::test]
    async fn a_weighted_call_until_returns_the_result_of_its_work() {
        let limit = Limit::new(3);
        let weighted = limit.weighted(2).unwrap();
        let stop = Cancellation::new();
        let done = weighted.run_until(&stop, None, || ready(Ok::<_, ()>(2)));
        assert_eq!(done.await, Ok(2));
        let failed = weighted.run_until(&stop, None, || ready(Err::<(), _>("no")));
        assert_eq!(failed.await, Err(ScopeError::Failed("no")));
        assert_eq!(limit.available(), 3);
    }

    #[tokio::test]
    async fn a_cancelled_weighted_wait_lets_the_call_behind_it_run() {
        let limit = Limit::new(2);
        let mut holder = pin!(limit.run(pending::<()>));
        assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
        let stop = Cancellation::new();
        let mut large = pin!(
            limit
                .weighted(2)
                .unwrap()
                .run_until(&stop, None, || { ready(Ok::<_, ()>(())) })
        );
        assert_eq!(poll_once(large.as_mut()), Poll::Pending);
        let mut small = pin!(limit.run(|| ready(1)));
        let small_waker = Watched::new();
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Pending);

        stop.cancel();
        assert_eq!(large.await, Err(ScopeError::Cancelled));
        assert!(small_waker.woken());
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Ready(1));
        assert_eq!(limit.available(), 1);
    }

    #[tokio::test]
    async fn the_deadline_ends_a_weighted_wait_and_lets_the_call_behind_it_run() {
        let limit = Limit::new(3);
        let mut holder = pin!(limit.run(pending::<()>));
        assert_eq!(poll_once(holder.as_mut()), Poll::Pending);
        let deadline = time::now() + Duration::from_millis(20);
        let stop = Cancellation::new();
        let mut large = pin!(limit.weighted(3).unwrap().run_until(
            &stop,
            Some(deadline),
            || ready(Ok::<_, ()>(()))
        ));
        assert_eq!(poll_once(large.as_mut()), Poll::Pending);
        let mut small = pin!(limit.weighted(2).unwrap().run(|| ready(2)));
        let small_waker = Watched::new();
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Pending);

        assert_eq!(large.await, Err(ScopeError::DeadlineExceeded));
        assert!(small_waker.woken());
        assert_eq!(small_waker.poll(small.as_mut()), Poll::Ready(2));
    }

    #[tokio::test]
    async fn a_cancelled_weighted_call_takes_no_place_and_starts_no_work() {
        let limit = Limit::new(2);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let mut started = false;
        let result = limit
            .weighted(2)
            .unwrap()
            .run_until(&cancellation, None, || {
                started = true;
                ready(Ok::<_, ()>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
        assert!(!started);
        assert_eq!(limit.available(), 2);
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn no_schedule_runs_more_work_than_the_limit() {
    use adamantium::{
        Actor,
        testing::{self, Config},
    };

    let config = Config {
        seed: 182,
        iterations: 300,
        max_steps: 20_000,
        ..Config::default()
    };
    let checked = testing::check(config, || async {
        let limit = Limit::new(2);
        let running = Actor::new((0_usize, 0_usize));
        let workers = (0..4).map(|_| {
            let (limit, running) = (limit.clone(), running.clone());
            async move {
                limit
                    .run(|| async {
                        running.with(|(now, most)| {
                            *now += 1;
                            *most = (*most).max(*now);
                        });
                        running.with(|(now, _)| *now -= 1);
                    })
                    .await;
            }
        });
        testing::parallel(workers.collect::<Vec<_>>()).await;
        let (now, most) = running.read(|state| *state);
        assert_eq!(now, 0);
        assert!(most <= 2, "{most} ran at once");
        assert_eq!(limit.available(), 2);
    });
    assert!(checked.is_ok(), "{checked:?}");
}

#[cfg(feature = "deterministic")]
mod model {
    use super::*;
    use adamantium::{
        Actor, Cancellation, ScopeError, Tasks,
        testing::{self, Config, TimelineEventKind},
        time,
    };
    use std::time::Duration;

    type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

    fn config(seed: u64) -> Config {
        Config {
            seed,
            iterations: 300,
            max_steps: 20_000,
            ..Config::default()
        }
    }

    #[test]
    fn no_schedule_runs_more_weight_than_the_limit() {
        let checked = testing::check(config(257), || async {
            let limit = Limit::new(3);
            let running = Actor::new((0_usize, 0_usize));
            let workers = [1, 3, 2, 1, 2].map(|weight| {
                let (limit, running) = (limit.clone(), running.clone());
                async move {
                    limit
                        .weighted(weight)
                        .unwrap()
                        .run(|| async {
                            running.with(|(now, most)| {
                                *now += weight;
                                *most = (*most).max(*now);
                            });
                            running.with(|(now, _)| *now -= weight);
                        })
                        .await;
                }
            });
            testing::parallel(workers).await;
            let (now, most) = running.read(|state| *state);
            assert_eq!(now, 0);
            assert!(most <= 3, "{most} places taken at once");
            assert_eq!(limit.available(), 3);
        });
        assert!(checked.is_ok(), "{checked:?}");
    }

    // The holder can't finish until the small call runs, and the small call
    // can queue behind the large one, so only the large call giving up its
    // wait lets everything finish.
    #[test]
    fn a_large_call_that_gives_up_waiting_lets_the_calls_behind_it_run() {
        let checked = testing::check(config(258), || async {
            let limit = Limit::new(2);
            let stop = Cancellation::new();
            let signal = Channel::<()>::bounded(1);
            let holder: Worker = Box::pin({
                let (limit, signal) = (limit.clone(), signal.clone());
                async move {
                    limit.run(|| signal.receive()).await;
                }
            });
            let large: Worker = Box::pin({
                let (limit, stop) = (limit.clone(), stop.clone());
                async move {
                    let result = limit
                        .weighted(2)
                        .unwrap()
                        .run_until(&stop, None, || ready(Ok::<_, ()>(())))
                        .await;
                    assert!(matches!(result, Ok(()) | Err(ScopeError::Cancelled)));
                }
            });
            let small: Worker = Box::pin({
                let (limit, signal) = (limit.clone(), signal.clone());
                async move {
                    limit.run(|| signal.send(())).await.unwrap();
                }
            });
            let canceller: Worker = Box::pin(async move { stop.cancel() });
            testing::parallel([holder, large, small, canceller]).await;
            assert_eq!(limit.available(), 2);
        });
        assert!(checked.is_ok(), "{checked:?}");
    }

    #[test]
    fn a_woken_call_that_gives_up_passes_its_turn_on() {
        let checked = testing::check(config(259), || async {
            let limit = Limit::new(2);
            let stop = Cancellation::new();
            let holder: Worker = Box::pin({
                let limit = limit.clone();
                async move {
                    limit.weighted(2).unwrap().run(tokio::task::yield_now).await;
                }
            });
            let quitter: Worker = Box::pin({
                let (limit, stop) = (limit.clone(), stop.clone());
                async move {
                    let result = limit
                        .weighted(2)
                        .unwrap()
                        .run_until(&stop, None, || ready(Ok::<_, ()>(())))
                        .await;
                    assert!(matches!(result, Ok(()) | Err(ScopeError::Cancelled)));
                }
            });
            let last: Worker = Box::pin({
                let limit = limit.clone();
                async move { limit.run(|| ready(())).await }
            });
            let canceller: Worker = Box::pin(async move { stop.cancel() });
            testing::parallel([holder, quitter, last, canceller]).await;
            assert_eq!(limit.available(), 2);
        });
        assert!(checked.is_ok(), "{checked:?}");
    }

    // The holders take both places before the others arrive. Whichever of
    // them is served first may be the one that waits for the other, so
    // serving it must wake the other.
    #[test]
    fn serving_a_call_wakes_the_next_call_that_fits() {
        let checked = testing::check(config(260), || async {
            let limit = Limit::new(2);
            let signal = Channel::<()>::bounded(1);
            let holders = [(); 2].map(|()| -> Worker {
                let limit = limit.clone();
                Box::pin(async move {
                    limit.run(|| time::sleep(Duration::from_millis(2))).await;
                })
            });
            let receiver: Worker = Box::pin({
                let (limit, signal) = (limit.clone(), signal.clone());
                async move {
                    time::sleep(Duration::from_millis(1)).await;
                    limit.run(|| signal.receive()).await;
                }
            });
            let sender: Worker = Box::pin({
                let (limit, signal) = (limit.clone(), signal.clone());
                async move {
                    time::sleep(Duration::from_millis(1)).await;
                    limit.run(|| signal.send(())).await.unwrap();
                }
            });
            let [first, second] = holders;
            testing::parallel([first, second, receiver, sender]).await;
            assert_eq!(limit.available(), 2);
        });
        assert!(checked.is_ok(), "{checked:?}");
    }

    async fn queued_behind_a_holder() {
        let limit = Limit::new(1);
        let release = Channel::<()>::bounded(1);
        let mut tasks = Tasks::<()>::new();
        let (holder_limit, holder_release) = (limit.clone(), release.clone());
        tasks.spawn_named("holder", async move {
            holder_limit.run(|| holder_release.receive()).await;
            Ok(())
        });
        for name in ["first", "second"] {
            time::sleep(Duration::from_millis(1)).await;
            let limit = limit.clone();
            tasks.spawn_named(name, async move {
                limit.run(|| ready(())).await;
                Ok(())
            });
        }
        time::sleep(Duration::from_millis(1)).await;
        release.send(()).await.unwrap();
        tasks.join(Ok(())).await.unwrap();
    }

    #[test]
    fn the_timeline_shows_calls_waiting_for_places_and_who_woke_them() {
        let record = testing::check(
            Config {
                iterations: 1,
                ..Config::default()
            },
            queued_behind_a_holder,
        )
        .unwrap();
        let timeline = testing::explain(&record, queued_behind_a_holder);
        assert_eq!(timeline.failure(), None);
        assert!(timeline.waiting().is_empty(), "{timeline}");

        let waits: Vec<Option<String>> = timeline
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                TimelineEventKind::Wait {
                    operation: "limit.acquire",
                    task_name,
                    ..
                } => Some(task_name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            waits,
            [Some("first".to_owned()), Some("second".to_owned())],
            "{timeline}"
        );
        let wakeups: Vec<(Option<String>, Vec<Option<String>>)> = timeline
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                TimelineEventKind::Wakeup {
                    operation: "limit.acquire",
                    by_task_name,
                    target_task_names,
                    ..
                } => Some((by_task_name.clone(), target_task_names.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            wakeups,
            [
                (Some("holder".to_owned()), vec![Some("first".to_owned())]),
                (Some("first".to_owned()), vec![Some("second".to_owned())]),
            ],
            "{timeline}"
        );
    }
}
