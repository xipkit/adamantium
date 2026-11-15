//! These tests run with and without the `tokio` feature, so they poll by hand.

use adamantium::{Channel, Limit};
use std::{
    future::{Future, pending, ready},
    pin::{Pin, pin},
    task::{Context, Poll, Waker},
};

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
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
