//! These tests run with and without the `tokio` feature, so they poll by hand.

use adamantium::{Channel, race};
use std::{
    future::{Future, pending, ready},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Counts its polls and its drop, and never finishes.
struct Watched {
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Watched {
    fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (polls, drops) = (Arc::default(), Arc::default());
        let watched = Self {
            polls: Arc::clone(&polls),
            drops: Arc::clone(&drops),
        };
        (watched, polls, drops)
    }
}

impl Future for Watched {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u32> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        Poll::Pending
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn the_future_that_is_ready_wins() {
    let mut raced = pin!(race((pending(), ready(2), pending())));
    assert_eq!(poll_once(raced.as_mut()), Poll::Ready(2));
}

#[test]
fn the_earlier_future_wins_when_two_are_ready() {
    let mut raced = pin!(race((ready(1), ready(2))));
    assert_eq!(poll_once(raced.as_mut()), Poll::Ready(1));
}

#[test]
fn a_later_future_is_not_polled_once_an_earlier_one_is_ready() {
    let (watched, polls, _) = Watched::new();
    let mut raced = pin!(race((ready(1), watched)));
    assert_eq!(poll_once(raced.as_mut()), Poll::Ready(1));
    assert_eq!(polls.load(Ordering::Relaxed), 0);
}

#[test]
fn the_losers_are_dropped_when_the_race_ends() {
    let (watched, polls, drops) = Watched::new();
    let wins = Channel::<u32>::bounded(1);
    {
        let mut raced = pin!(race((watched, async { wins.receive().await.unwrap_or(0) })));
        assert_eq!(poll_once(raced.as_mut()), Poll::Pending);
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        wins.try_send(9).unwrap();
        assert_eq!(poll_once(raced.as_mut()), Poll::Ready(9));
        assert_eq!(polls.load(Ordering::Relaxed), 2);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn dropping_the_race_drops_every_future() {
    let (first, _, first_drops) = Watched::new();
    let (second, _, second_drops) = Watched::new();
    {
        let mut raced = pin!(race((first, second)));
        assert_eq!(poll_once(raced.as_mut()), Poll::Pending);
    }
    assert_eq!(first_drops.load(Ordering::Relaxed), 1);
    assert_eq!(second_drops.load(Ordering::Relaxed), 1);
}

#[test]
fn a_loser_keeps_a_message_it_did_not_take() {
    let jobs = Channel::<u32>::bounded(1);
    let stop = Channel::<()>::bounded(1);
    stop.try_send(()).unwrap();
    {
        let mut raced = pin!(race((async { stop.receive().await.map(|()| 0) }, async {
            jobs.receive().await
        },)));
        assert_eq!(poll_once(raced.as_mut()), Poll::Ready(Some(0)));
    }
    jobs.try_send(5).unwrap();
    assert_eq!(jobs.try_receive(), Ok(5));
}

#[test]
fn futures_can_borrow_and_have_different_types() {
    #[derive(Debug, PartialEq)]
    enum Event {
        Text(usize),
        Number(u32),
    }

    let text = String::from("borrowed");
    let number = Channel::<u32>::bounded(1);
    let mut raced = pin!(race((
        async { Event::Number(number.receive().await.unwrap_or(0)) },
        async { Event::Text(text.len()) },
    )));
    assert_eq!(poll_once(raced.as_mut()), Poll::Ready(Event::Text(8)));
}

#[test]
fn eight_futures_can_race() {
    let mut raced = pin!(race((
        pending(),
        pending(),
        pending(),
        pending(),
        pending(),
        pending(),
        pending(),
        ready(8),
    )));
    assert_eq!(poll_once(raced.as_mut()), Poll::Ready(8));
}

#[test]
fn a_race_of_send_futures_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let number = Channel::<u32>::bounded(1);
    let raced = race((async move { number.receive().await }, ready(None)));
    assert_send(&raced);
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn a_task_wakes_when_a_raced_receive_gets_a_message() {
    use adamantium::Tasks;

    let jobs = Channel::<u32>::bounded(1);
    let stop = Channel::<u32>::bounded(1);
    let result = Tasks::<()>::run(async |tasks| {
        let sender = jobs.clone();
        let sent = tasks.spawn(async move {
            sender.send(3).await.ok();
            Ok(())
        });
        let first = race((async { stop.receive().await }, async {
            jobs.receive().await
        }))
        .await;
        tasks.value(sent).await?;
        Ok(first)
    })
    .await;
    assert_eq!(result, Ok(Some(3)));
}

#[cfg(feature = "tokio")]
mod until_tests {
    use super::*;
    use adamantium::{Cancellation, UntilError, time, until};
    use std::time::Duration;
    use tokio::time::Instant;

    fn past() -> Option<Instant> {
        Some(time::now() - Duration::from_millis(1))
    }

    #[tokio::test]
    async fn returns_output_when_future_completes() {
        let cancellation = Cancellation::new();
        let ch = Channel::bounded(1);
        ch.try_send(42).unwrap();
        assert_eq!(until(&cancellation, None, ch.receive()).await, Ok(Some(42)));
    }

    #[cfg(not(target_os = "wasi"))]
    #[tokio::test(start_paused = true)]
    async fn ends_at_deadline_and_drops_the_future() {
        let cancellation = Cancellation::new();
        let (watched, _polls, drops) = Watched::new();
        let start = time::now();
        let deadline = start + Duration::from_secs(5);
        assert_eq!(
            until(&cancellation, Some(deadline), watched).await,
            Err(UntilError::DeadlineExceeded)
        );
        assert!(time::now() >= deadline);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ends_when_cancelled_and_prefers_cancellation_to_ready_future() {
        let cancellation = Cancellation::new();
        let (watched, polls, drops) = Watched::new();
        {
            let mut bounded = pin!(until(&cancellation, None, watched));
            assert_eq!(poll_once(bounded.as_mut()), Poll::Pending);
            assert_eq!(polls.load(Ordering::Relaxed), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 0);

            cancellation.cancel();
            assert_eq!(
                poll_once(bounded.as_mut()),
                Poll::Ready(Err(UntilError::Cancelled))
            );
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        }

        // A cancellation already present wins without polling the wrapped future.
        let (watched, polls, _) = Watched::new();
        assert_eq!(
            until(&cancellation, None, watched).await,
            Err(UntilError::Cancelled)
        );
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert_eq!(
            until(&cancellation, None, ready(7)).await,
            Err(UntilError::Cancelled)
        );
    }

    #[tokio::test]
    async fn prefers_cancellation_to_a_passed_deadline() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(
            until(&cancellation, past(), pending::<()>()).await,
            Err(UntilError::Cancelled)
        );
        assert_eq!(
            until(&Cancellation::new(), past(), pending::<()>()).await,
            Err(UntilError::DeadlineExceeded)
        );
    }

    #[test]
    fn is_send_when_wrapped_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let cancellation = Cancellation::new();
        let ch = Channel::<u32>::bounded(1);
        let bounded = until(&cancellation, None, ch.receive());
        assert_send(&bounded);
    }

    #[test]
    fn error_display_messages() {
        assert_eq!(UntilError::Cancelled.to_string(), "wait cancelled");
        assert_eq!(
            UntilError::DeadlineExceeded.to_string(),
            "wait deadline exceeded"
        );
    }
}
