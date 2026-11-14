//! These tests run with and without the `tokio` feature, so the
//! latest-value type stays available to crates that leave Tokio out.

#[cfg(feature = "tokio")]
mod support;

#[cfg(feature = "tokio")]
use adamantium::{Cancellation, ReceiveError, Tasks, time};
use adamantium::{Closed, TryReceiveError, Watch, Watcher};
#[cfg(feature = "tokio")]
use std::time::Duration;
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl WakeCounter {
    fn wakes(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>, counter: &Arc<WakeCounter>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::clone(counter));
    future.poll(&mut Context::from_waker(&waker))
}

/// Polls a new future once.
fn now<F: Future>(future: F) -> Poll<F::Output> {
    poll_once(pin!(future), &Arc::new(WakeCounter::default()))
}

#[test]
fn a_new_watcher_has_seen_the_current_value() {
    let watch = Watch::new(1);
    let mut watcher = watch.subscribe();
    assert_eq!(watcher.get(), 1);
    assert!(!watcher.has_changed());
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(now(watcher.receive()), Poll::Pending);
}

#[test]
fn a_watcher_receives_the_latest_value_once() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    watch.send(1).unwrap();
    watch.send(2).unwrap();
    watch.send(3).unwrap();
    assert!(watcher.has_changed());
    assert_eq!(watcher.clone().get(), 3);
    assert!(watcher.has_changed(), "a clone's get is its own");
    assert_eq!(now(watcher.receive()), Poll::Ready(Some(3)));
    assert!(!watcher.has_changed());
    assert_eq!(now(watcher.receive()), Poll::Pending);
    watch.send(4).unwrap();
    assert_eq!(watcher.try_receive(), Ok(4));
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Empty));
}

#[test]
fn get_marks_the_value_seen() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    watch.send(1).unwrap();
    assert_eq!(watcher.get(), 1);
    assert!(!watcher.has_changed());
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Empty));
    watch.send(2).unwrap();
    assert_eq!(watcher.try_receive(), Ok(2));
}

#[test]
fn sending_the_same_value_again_is_a_change() {
    let watch = Watch::new(1);
    let mut watcher = watch.subscribe();
    watch.send(1).unwrap();
    assert_eq!(watcher.try_receive(), Ok(1));
}

#[test]
fn a_send_wakes_every_waiting_watcher() {
    let watch = Watch::new(0);
    let mut first = watch.subscribe();
    let mut second = watch.subscribe();
    let counters = [
        Arc::new(WakeCounter::default()),
        Arc::new(WakeCounter::default()),
    ];
    let mut first_waits = pin!(first.receive());
    let mut second_waits = pin!(second.receive());
    assert_eq!(poll_once(first_waits.as_mut(), &counters[0]), Poll::Pending);
    assert_eq!(
        poll_once(second_waits.as_mut(), &counters[1]),
        Poll::Pending
    );
    watch.send(7).unwrap();
    assert!(counters.iter().all(|counter| counter.wakes() > 0));
    assert_eq!(
        poll_once(first_waits.as_mut(), &counters[0]),
        Poll::Ready(Some(7))
    );
    assert_eq!(
        poll_once(second_waits.as_mut(), &counters[1]),
        Poll::Ready(Some(7))
    );
}

#[test]
fn each_wait_is_woken_by_the_next_send() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let counter = Arc::new(WakeCounter::default());
    for value in 1..=3 {
        let mut waiting = pin!(watcher.receive());
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
        let wakes = counter.wakes();
        watch.send(value).unwrap();
        assert!(counter.wakes() > wakes);
        assert_eq!(
            poll_once(waiting.as_mut(), &counter),
            Poll::Ready(Some(value))
        );
    }
}

#[test]
fn dropping_a_waiting_receive_leaves_the_value_unseen() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let counter = Arc::new(WakeCounter::default());
    {
        let mut waiting = pin!(watcher.receive());
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
        watch.send(1).unwrap();
    }
    assert!(watcher.has_changed());
    assert_eq!(watcher.try_receive(), Ok(1));
}

#[test]
fn clones_of_a_watcher_keep_their_own_place() {
    let watch = Watch::new(0);
    let mut first = watch.subscribe();
    watch.send(1).unwrap();
    let mut second = first.clone();
    assert!(
        second.has_changed(),
        "a clone starts where the original was"
    );
    assert_eq!(first.try_receive(), Ok(1));
    assert!(
        second.has_changed(),
        "the original's receive isn't the clone's"
    );
    assert_eq!(second.try_receive(), Ok(1));
    assert_eq!(second.try_receive(), Err(TryReceiveError::Empty));
}

#[test]
fn a_late_watcher_reads_a_value_sent_with_no_watchers() {
    let watch = Watch::new("old");
    assert_eq!(watch.send("new"), Ok(()));
    assert_eq!(watch.get(), "new");
    let mut watcher = watch.subscribe();
    assert_eq!(watcher.get(), "new");
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Empty));
}

#[test]
fn closing_ends_waits_after_the_last_value() {
    let watch = Watch::new(0);
    let mut behind = watch.subscribe();
    let mut waiting = watch.subscribe();
    let counter = Arc::new(WakeCounter::default());
    let mut wait = pin!(waiting.receive());
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Pending);
    watch.send(1).unwrap();
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Ready(Some(1)));

    let mut caught_up = watch.subscribe();
    let mut wait = pin!(caught_up.receive());
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Pending);
    let wakes = counter.wakes();
    assert!(watch.close());
    assert!(counter.wakes() > wakes);
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Ready(None));

    assert!(!watch.close());
    assert!(watch.is_closed());
    assert!(behind.is_closed());
    assert_eq!(watch.send(2), Err(Closed(2)));
    assert_eq!(watch.get(), 1, "the last value stays readable");
    assert_eq!(
        behind.try_receive(),
        Ok(1),
        "an unseen last value comes first"
    );
    assert_eq!(behind.try_receive(), Err(TryReceiveError::Closed));
    assert_eq!(now(behind.receive()), Poll::Ready(None));
}

#[test]
fn dropping_the_last_watch_closes_it() {
    let watch = Watch::new(0);
    let other = watch.clone();
    let mut watcher = watch.subscribe();
    other.send(1).unwrap();
    drop(watch);
    assert!(!watcher.is_closed(), "a clone still holds it open");
    assert_eq!(watcher.try_receive(), Ok(1));

    let counter = Arc::new(WakeCounter::default());
    let mut waiting = pin!(watcher.receive());
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
    drop(other);
    assert!(counter.wakes() > 0);
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Ready(None));
}

#[test]
fn dropping_every_watcher_leaves_the_watch_open() {
    let watch = Watch::new(0);
    drop(watch.subscribe());
    assert!(!watch.is_closed());
    assert_eq!(watch.send(1), Ok(()));
}

#[test]
fn wait_for_checks_the_current_value_first() {
    let watch = Watch::new(5);
    let mut watcher = watch.subscribe();
    assert_eq!(
        now(watcher.wait_for(|n| *n == 5)),
        Poll::Ready(Some(5)),
        "a value already seen still counts"
    );
}

#[test]
fn wait_for_checks_each_value_once() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    watch.send(1).unwrap();
    let counter = Arc::new(WakeCounter::default());
    let mut checked = Vec::new();
    {
        let mut waiting = pin!(watcher.wait_for(|n| {
            checked.push(*n);
            *n >= 3
        }));
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
        watch.send(2).unwrap();
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
        watch.send(3).unwrap();
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Ready(Some(3)));
    }
    // The unseen current value is checked once, not again as a change.
    assert_eq!(checked, [1, 2, 3]);
    assert!(!watcher.has_changed(), "checked values are marked seen");
}

#[test]
fn wait_for_ends_when_the_watch_closes() {
    let watch = Watch::new(false);
    let mut watcher = watch.subscribe();
    let counter = Arc::new(WakeCounter::default());
    let mut waiting = pin!(watcher.wait_for(|ready| *ready));
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
    watch.close();
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Ready(None));
}

#[test]
fn wait_for_sees_a_last_value_sent_before_closing() {
    let watch = Watch::new(false);
    let mut watcher = watch.subscribe();
    watch.send(true).unwrap();
    watch.close();
    assert_eq!(
        now(watcher.wait_for(|ready| *ready)),
        Poll::Ready(Some(true))
    );
}

#[test]
fn a_watch_dropped_by_a_panic_still_closes() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = watch;
        std::panic::panic_any("unwinding");
    }));
    assert!(unwound.is_err());
    assert!(watcher.is_closed());
    assert_eq!(watcher.try_receive(), Err(TryReceiveError::Closed));
}

/// A value whose destructor reads the watch it was stored in.
#[derive(Clone)]
struct ReadsOnDrop(Option<Watcher<ReadsOnDrop>>);

impl Drop for ReadsOnDrop {
    fn drop(&mut self) {
        if let Some(watcher) = self.0.take() {
            // Takes the watch's lock, so it would deadlock if a send
            // dropped the old value while holding it.
            let _ = watcher.is_closed();
        }
    }
}

#[test]
fn a_replaced_value_is_dropped_outside_the_lock() {
    let watch = Watch::new(ReadsOnDrop(None));
    assert!(watch.send(ReadsOnDrop(Some(watch.subscribe()))).is_ok());
    assert!(watch.send(ReadsOnDrop(None)).is_ok());
    assert!(!watch.is_closed());
}

#[test]
fn handles_cross_tasks_and_describe_themselves() {
    fn crosses_tasks<T: Send + Sync + 'static>() {}
    crosses_tasks::<Watch<String>>();
    crosses_tasks::<Watcher<String>>();

    let watch = Watch::new(1);
    let watcher = watch.subscribe();
    assert_eq!(format!("{watch:?}"), "Watch { value: 1, closed: false }");
    watch.send(2).unwrap();
    watch.close();
    assert_eq!(
        format!("{watcher:?}"),
        "Watcher { value: 2, changed: true, closed: true }"
    );
}

#[cfg(feature = "tokio")]
fn past() -> Option<Instant> {
    Some(time::now() - Duration::from_millis(1))
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "multi_thread")]
async fn every_watcher_ends_on_the_last_value() {
    let watch = Watch::new(0_u32);
    let last = Tasks::<&str>::run(async |t| {
        let mut readers = Vec::new();
        for _ in 0..4 {
            let mut watcher = watch.subscribe();
            readers.push(t.spawn(async move {
                let mut previous = watcher.get();
                while let Some(value) = watcher.receive().await {
                    // A value read by `get` isn't received again.
                    if value <= previous {
                        return Err("a value repeated or went backwards");
                    }
                    previous = value;
                }
                Ok(previous)
            }));
        }
        let writer = watch.clone();
        t.spawn(async move {
            for n in 1..=1000 {
                writer.send(n).map_err(|_| "closed")?;
                tokio::task::yield_now().await;
            }
            writer.close();
            Ok(())
        });
        let mut last = Vec::new();
        for reader in readers {
            last.push(t.value(reader).await?);
        }
        Ok(last)
    })
    .await;
    assert_eq!(last, Ok(vec![1000; 4]));
}

#[cfg(feature = "tokio")]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_at_the_deadline_and_leaves_later_values() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let cancellation = Cancellation::new();
    let start = time::now();
    assert_eq!(
        watcher
            .receive_until(&cancellation, Some(start + Duration::from_secs(5)))
            .await,
        Err(ReceiveError::DeadlineExceeded)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    watch.send(1).unwrap();
    assert_eq!(watcher.receive_until(&cancellation, None).await, Ok(1));
    watch.close();
    assert_eq!(
        watcher.receive_until(&cancellation, None).await,
        Err(ReceiveError::Closed)
    );
}

#[cfg(feature = "tokio")]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_when_cancelled() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    let cancellation = Cancellation::new();
    let start = time::now();
    let canceller = cancellation.clone();
    let (received, ()) = tokio::join!(
        watcher.receive_until(&cancellation, Some(start + Duration::from_secs(3600))),
        async move {
            time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        },
    );
    assert_eq!(received, Err(ReceiveError::Cancelled));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn receive_until_prefers_shutdown_to_an_unseen_value() {
    let watch = Watch::new(0);
    let mut watcher = watch.subscribe();
    watch.send(1).unwrap();
    let cancellation = Cancellation::new();
    // Repeated, so an unordered race between the two would show.
    for _ in 0..16 {
        assert_eq!(
            watcher.receive_until(&cancellation, past()).await,
            Err(ReceiveError::DeadlineExceeded)
        );
    }
    cancellation.cancel();
    for _ in 0..16 {
        assert_eq!(
            watcher.receive_until(&cancellation, past()).await,
            Err(ReceiveError::Cancelled)
        );
    }
    assert!(watcher.has_changed(), "shutdown left the value unseen");
    assert_eq!(watcher.try_receive(), Ok(1));
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_racing_a_new_wait_is_never_missed() {
    // A wait that checks, then listens, can miss a send made between the
    // two and sleep forever. Each round races a send against a fresh wait.
    let watch = Watch::new(0_u32);
    let result = Tasks::<&str>::run(async |t| {
        for round in 1..=20_000 {
            let mut watcher = watch.subscribe();
            let waiting = t.spawn(async move { Ok(watcher.receive().await) });
            let writer = watch.clone();
            t.spawn(async move { writer.send(round).map_err(|_| "closed") });
            let received = support::timeout(Duration::from_secs(5), t.value(waiting))
                .await
                .map_err(|_| "a wait missed its send")??;
            if received != Some(round) {
                return Err("received the wrong value");
            }
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
}
