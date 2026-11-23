#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    reason = "outcome counts shared across schedules"
)]

use std::{
    collections::BTreeMap,
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Wake, Waker},
};

use adamantium::{
    Cancellation, Channel, Signal, race,
    testing::{self, Config, DfsStatus, SchedulerMode},
};
use futures_util::FutureExt;

type Worker<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Outcomes = Arc<Mutex<BTreeMap<usize, usize>>>;

fn config() -> Config {
    Config {
        seed: 255,
        iterations: 1_000,
        max_steps: 20_000,
        ..Config::default()
    }
}

#[allow(
    clippy::unwrap_used,
    reason = "test schedule failures must fail the test"
)]
fn check<F, Fut>(factory: F)
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

fn record(outcomes: &Outcomes, outcome: usize) {
    let mut outcomes = outcomes.lock().unwrap_or_else(PoisonError::into_inner);
    *outcomes.entry(outcome).or_default() += 1;
}

fn pending(signal: &Signal) -> bool {
    signal.notified().now_or_never().is_some()
}

#[test]
fn a_notify_racing_the_start_of_a_wait_is_never_missed() {
    let exploration = Config {
        iterations: 10_000,
        max_steps: 2_000,
        ..Config::default()
    }
    .with_scheduler(SchedulerMode::Dfs);
    let record = testing::check(exploration, || async {
        let signal = Signal::new();
        let notifier = signal.clone();
        let workers: Vec<Worker<()>> = vec![
            Box::pin(async move { signal.notified().await }),
            Box::pin(async move { notifier.notify() }),
        ];
        testing::parallel(workers).await;
    })
    .unwrap();
    assert_eq!(record.dfs_status(), Some(DfsStatus::Complete));
}

/// Wakes the task through a flag, so a test can tell which wait woke it.
struct Flag {
    woken: Arc<AtomicBool>,
    task: Waker,
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.task.wake_by_ref();
    }
}

/// Runs `future`, setting `woken` when it wakes the task.
async fn flagged<F: Future>(future: F, woken: &Arc<AtomicBool>) -> F::Output {
    let mut future = pin!(future);
    poll_fn(|cx| {
        let waker = Waker::from(Arc::new(Flag {
            woken: Arc::clone(woken),
            task: cx.waker().clone(),
        }));
        future.as_mut().poll(&mut Context::from_waker(&waker))
    })
    .await
}

const YIELDS: usize = 8;
const CANCELLED: usize = 0;
const TOOK: usize = 1;
const WOKEN_THEN_DROPPED: usize = 2;

#[test]
fn a_woken_waiter_dropped_by_race_passes_the_notification_on() {
    let outcomes = Outcomes::default();
    let seen = Arc::clone(&outcomes);
    check(move || {
        let seen = Arc::clone(&seen);
        async move {
            let signal = Signal::new();
            let cancellation = Cancellation::new();
            let (racing, plain, notifier) = (signal.clone(), signal.clone(), signal.clone());
            let (stop, canceller) = (cancellation.clone(), cancellation);
            let workers: Vec<Worker<()>> = vec![
                Box::pin(async move {
                    let woken = Arc::new(AtomicBool::new(false));
                    // Cancellation comes first, so it wins when both are
                    // ready and a woken wait is dropped.
                    let took = race((
                        async {
                            stop.cancelled().await;
                            false
                        },
                        async {
                            flagged(racing.notified(), &woken).await;
                            true
                        },
                    ))
                    .await;
                    let outcome = if took {
                        // Hand the plain waiter a notification of its own.
                        racing.notify();
                        TOOK
                    } else if woken.load(Ordering::SeqCst) {
                        WOKEN_THEN_DROPPED
                    } else {
                        CANCELLED
                    };
                    record(&seen, outcome);
                }),
                Box::pin(async move {
                    // Start late, so the racing waiter often listens first
                    // and is the one the notify wakes.
                    for _ in 0..YIELDS {
                        tokio::task::yield_now().await;
                    }
                    plain.notified().await;
                }),
                Box::pin(async move {
                    // Notify once both are likely listening, and cancel
                    // straight after, before the woken waiter runs.
                    for _ in 0..YIELDS {
                        tokio::task::yield_now().await;
                    }
                    notifier.notify();
                    canceller.cancel();
                }),
            ];
            testing::parallel(workers).await;
            // The plain waiter took the first notification or the one
            // handed to it, and nothing is left over.
            assert!(!pending(&signal), "a notification was taken twice");
        }
    });
    let outcomes = outcomes.lock().unwrap();
    assert!(outcomes.get(&TOOK).is_some_and(|n| *n > 0), "{outcomes:?}");
    assert!(
        outcomes.get(&WOKEN_THEN_DROPPED).is_some_and(|n| *n > 0),
        "{outcomes:?}"
    );
}

#[test]
fn one_notify_releases_exactly_one_of_several_waiters() {
    // Each released waiter reports itself before the next notify. A notify
    // that released two would leave the last notify with nobody to take
    // it, and one that released none would deadlock.
    check(|| async {
        let signal = Signal::new();
        let (released, reports) = Channel::bounded(3).split();
        let mut workers: Vec<Worker<()>> = (0..3)
            .map(|_| {
                let (signal, released) = (signal.clone(), released.clone());
                Box::pin(async move {
                    signal.notified().await;
                    released.send(()).await.unwrap();
                }) as Worker<()>
            })
            .collect();
        drop(released);
        let notifier = signal.clone();
        workers.push(Box::pin(async move {
            for _ in 0..3 {
                notifier.notify();
                reports.receive().await.unwrap();
            }
        }));
        testing::parallel(workers).await;
        assert!(!pending(&signal), "one notify released two waiters");
    });
}

#[test]
fn notifies_merge_only_while_one_is_pending() {
    // The waiter takes one notification. The second notify either merged
    // into the first, or came after the take and is still pending.
    let outcomes = Outcomes::default();
    let seen = Arc::clone(&outcomes);
    check(move || {
        let seen = Arc::clone(&seen);
        async move {
            let signal = Signal::new();
            let (waiter, notifier) = (signal.clone(), signal.clone());
            let workers: Vec<Worker<()>> = vec![
                Box::pin(async move { waiter.notified().await }),
                Box::pin(async move {
                    notifier.notify();
                    notifier.notify();
                }),
            ];
            testing::parallel(workers).await;
            record(&seen, usize::from(pending(&signal)));
        }
    });
    let outcomes = outcomes.lock().unwrap();
    assert!(outcomes.get(&0).is_some_and(|n| *n > 0), "{outcomes:?}");
    assert!(outcomes.get(&1).is_some_and(|n| *n > 0), "{outcomes:?}");
}
