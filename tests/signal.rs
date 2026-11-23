#[cfg(all(feature = "tokio", not(target_os = "wasi")))]
mod support;

#[cfg(all(feature = "tokio", not(target_os = "wasi")))]
use adamantium::Tasks;
#[cfg(feature = "tokio")]
use adamantium::{Cancellation, UntilError, time};
use adamantium::{Signal, race};
#[cfg(feature = "tokio")]
use std::time::Duration;
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A future polled only through its own waker, which counts its wakes.
/// Polling it with another waker would replace the one the signal wakes.
struct Watched<F> {
    future: Pin<Box<F>>,
    wakes: Arc<WakeCounter>,
    waker: Waker,
}

impl<F: Future> Watched<F> {
    fn new(future: F) -> Self {
        let wakes = Arc::new(WakeCounter::default());
        Self {
            future: Box::pin(future),
            waker: Waker::from(Arc::clone(&wakes)),
            wakes,
        }
    }

    fn poll(&mut self) -> Poll<F::Output> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    fn wakes(&self) -> usize {
        self.wakes.0.load(Ordering::SeqCst)
    }
}

fn now<F: Future>(future: F) -> Poll<F::Output> {
    pin!(future).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn notifications_sent_before_a_wait_merge_into_one() {
    let signal = Signal::new();
    assert_eq!(now(signal.notified()), Poll::Pending);
    for _ in 0..3 {
        signal.notify();
    }
    assert_eq!(now(signal.notified()), Poll::Ready(()));
    assert_eq!(now(signal.notified()), Poll::Pending);
}

#[test]
fn a_notify_wakes_a_waiting_call_which_takes_it() {
    let signal = Signal::new();
    let mut waiting = Watched::new(signal.notified());
    assert_eq!(waiting.poll(), Poll::Pending);
    assert_eq!(waiting.wakes(), 0);
    signal.notify();
    assert_eq!(waiting.wakes(), 1);
    assert_eq!(waiting.poll(), Poll::Ready(()));
    assert_eq!(now(signal.notified()), Poll::Pending, "the wait took it");
}

#[test]
fn a_notify_wakes_one_of_several_waiting_calls() {
    let signal = Signal::new();
    let mut waiting: Vec<_> = (0..3).map(|_| Watched::new(signal.notified())).collect();
    for call in &mut waiting {
        assert_eq!(call.poll(), Poll::Pending);
    }
    signal.notify();
    let woken: Vec<usize> = waiting.iter().map(Watched::wakes).collect();
    assert_eq!(woken.iter().sum::<usize>(), 1, "{woken:?}");
    let ready = waiting
        .iter_mut()
        .filter_map(|call| call.poll().is_ready().then_some(()))
        .count();
    assert_eq!(ready, 1);
}

#[test]
fn notifications_sent_while_one_is_pending_release_one_waiting_call() {
    let signal = Signal::new();
    let mut waiting: Vec<_> = (0..3).map(|_| Watched::new(signal.notified())).collect();
    for call in &mut waiting {
        assert_eq!(call.poll(), Poll::Pending);
    }
    for _ in 0..3 {
        signal.notify();
    }
    let woken: Vec<usize> = waiting.iter().map(Watched::wakes).collect();
    assert_eq!(woken.iter().sum::<usize>(), 1, "{woken:?}");
    let ready = waiting
        .iter_mut()
        .filter_map(|call| call.poll().is_ready().then_some(()))
        .count();
    assert_eq!(ready, 1);
}

#[test]
fn a_call_that_starts_before_the_woken_one_runs_takes_the_notification() {
    let signal = Signal::new();
    let mut woken = Watched::new(signal.notified());
    assert_eq!(woken.poll(), Poll::Pending);
    signal.notify();
    assert_eq!(woken.wakes(), 1);
    assert_eq!(now(signal.notified()), Poll::Ready(()));
    assert_eq!(woken.poll(), Poll::Pending, "the woken call keeps waiting");
    signal.notify();
    assert_eq!(woken.wakes(), 2);
    assert_eq!(woken.poll(), Poll::Ready(()));
}

#[test]
fn dropping_a_woken_call_passes_its_wakeup_to_another() {
    let signal = Signal::new();
    let mut first = Watched::new(signal.notified());
    let mut second = Watched::new(signal.notified());
    assert_eq!(first.poll(), Poll::Pending);
    assert_eq!(second.poll(), Poll::Pending);
    signal.notify();
    assert_eq!(first.wakes() + second.wakes(), 1);
    let (woken, mut other) = if first.wakes() == 1 {
        (first, second)
    } else {
        (second, first)
    };
    drop(woken);
    assert_eq!(other.wakes(), 1, "the wakeup passed on");
    assert_eq!(other.poll(), Poll::Ready(()));
}

#[test]
fn dropping_a_waiting_call_leaves_the_notification_pending() {
    let signal = Signal::new();
    let mut waiting = Watched::new(signal.notified());
    assert_eq!(waiting.poll(), Poll::Pending);
    signal.notify();
    drop(waiting);
    assert_eq!(now(signal.notified()), Poll::Ready(()));
}

/// Ready once `stop` is set. It registers no waker, so the test polls by hand.
fn stopped(stop: &AtomicBool) -> impl Future<Output = &'static str> + '_ {
    poll_fn(|_| {
        if stop.load(Ordering::SeqCst) {
            Poll::Ready("stopped")
        } else {
            Poll::Pending
        }
    })
}

#[test]
fn a_woken_wait_that_loses_a_race_passes_its_wakeup_on() {
    let signal = Signal::new();
    let stops = [AtomicBool::new(false), AtomicBool::new(false)];
    let mut raced: Vec<_> = stops
        .iter()
        .map(|stop| {
            Watched::new(race((stopped(stop), async {
                signal.notified().await;
                "notified"
            })))
        })
        .collect();
    for call in &mut raced {
        assert_eq!(call.poll(), Poll::Pending);
    }
    signal.notify();
    let woken = raced
        .iter()
        .position(|call| call.wakes() == 1)
        .expect("a call was woken");
    let other = if woken == 0 { 1 } else { 0 };
    assert_eq!(raced[other].wakes(), 0);
    // Both futures are ready, so the earlier one wins and the woken wait is
    // dropped without taking the notification.
    stops[woken].store(true, Ordering::SeqCst);
    assert_eq!(raced[woken].poll(), Poll::Ready("stopped"));
    assert_eq!(raced[other].wakes(), 1, "the wakeup passed on");
    assert_eq!(raced[other].poll(), Poll::Ready("notified"));
}

#[test]
fn clones_share_one_notification() {
    let signal = Signal::new();
    let other = signal.clone();
    other.notify();
    signal.notify();
    assert_eq!(now(signal.notified()), Poll::Ready(()));
    assert_eq!(now(other.notified()), Poll::Pending);
}

#[test]
fn a_signal_crosses_tasks_and_describes_itself() {
    fn crosses_tasks<T: Send + Sync + std::panic::UnwindSafe + 'static>() {}
    fn is_send<T: Send>(_: &T) {}
    crosses_tasks::<Signal>();
    let signal = Signal::default();
    is_send(&signal.notified());
    assert_eq!(format!("{signal:?}"), "Signal { pending: false }");
    signal.notify();
    assert_eq!(format!("{signal:?}"), "Signal { pending: true }");
}

#[cfg(feature = "tokio")]
fn past() -> Option<Instant> {
    Some(time::now() - Duration::from_millis(1))
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn notified_until_ends_at_the_deadline_and_leaves_later_notifications() {
    let signal = Signal::new();
    let cancellation = Cancellation::new();
    let start = time::now();
    assert_eq!(
        signal
            .notified_until(&cancellation, Some(start + Duration::from_secs(5)))
            .await,
        Err(UntilError::DeadlineExceeded)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    signal.notify();
    assert_eq!(signal.notified_until(&cancellation, None).await, Ok(()));
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn notified_until_ends_when_cancelled() {
    let signal = Signal::new();
    let cancellation = Cancellation::new();
    let start = time::now();
    let canceller = cancellation.clone();
    let (notified, ()) = tokio::join!(
        signal.notified_until(&cancellation, Some(start + Duration::from_secs(3600))),
        async move {
            time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        },
    );
    assert_eq!(notified, Err(UntilError::Cancelled));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn notified_until_prefers_shutdown_to_a_pending_notification() {
    let signal = Signal::new();
    signal.notify();
    let cancellation = Cancellation::new();
    for _ in 0..16 {
        assert_eq!(
            signal.notified_until(&cancellation, past()).await,
            Err(UntilError::DeadlineExceeded)
        );
    }
    cancellation.cancel();
    for _ in 0..16 {
        assert_eq!(
            signal.notified_until(&cancellation, past()).await,
            Err(UntilError::Cancelled)
        );
    }
    assert_eq!(now(signal.notified()), Poll::Ready(()), "still pending");
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn notified_until_wakes_on_a_notify() {
    let signal = Signal::new();
    let cancellation = Cancellation::new();
    let notifier = signal.clone();
    let (notified, ()) = tokio::join!(signal.notified_until(&cancellation, None), async move {
        tokio::task::yield_now().await;
        notifier.notify();
    });
    assert_eq!(notified, Ok(()));
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notify_racing_a_new_wait_is_never_missed() {
    let signal = Signal::new();
    let result = Tasks::<&str>::run(async |t| {
        for _ in 0..20_000 {
            let waiter = signal.clone();
            let waiting = t.spawn(async move {
                waiter.notified().await;
                Ok(())
            });
            let notifier = signal.clone();
            t.spawn(async move {
                notifier.notify();
                Ok(())
            });
            support::timeout(Duration::from_secs(5), t.value(waiting))
                .await
                .map_err(|_| "a wait missed its notification")??;
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_notification_reaches_a_worker_pool() {
    // Each producer adds one item and notifies. Workers take every item they
    // find after each wakeup, so merged notifications lose no item.
    const ITEMS: usize = 10_000;
    let queued = adamantium::Actor::new(0_usize);
    let taken = adamantium::Actor::new(0_usize);
    let arrived = Signal::new();
    let stop = Cancellation::new();
    let result = Tasks::<&str>::run(async |t| {
        let mut workers = Vec::new();
        for _ in 0..3 {
            let (queued, taken, arrived, stop) =
                (queued.clone(), taken.clone(), arrived.clone(), stop.clone());
            workers.push(t.spawn(async move {
                while arrived.notified_until(&stop, None).await.is_ok() {
                    let items = queued.with(std::mem::take);
                    taken.with(|taken| *taken += items);
                }
                Ok(())
            }));
        }
        for _ in 0..ITEMS {
            queued.with(|queued| *queued += 1);
            arrived.notify();
            tokio::task::yield_now().await;
        }
        support::timeout(Duration::from_secs(10), async {
            while taken.read(|taken| *taken) < ITEMS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "an item was never taken")?;
        stop.cancel();
        for worker in workers {
            t.value(worker).await?;
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(taken.snapshot(), ITEMS);
}
