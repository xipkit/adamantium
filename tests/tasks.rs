#![cfg(feature = "tokio")]

use std::{
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use adamantium::{Tasks, blocking, concurrently};
use tokio::{
    sync::{Barrier, Semaphore, oneshot},
    time::{sleep, timeout},
};

/// How long a test waits before calling a join hung. No test relies on it for
/// ordering; it only turns a hang into a failure.
const HUNG: Duration = Duration::from_secs(30);

/// Sets a flag when dropped, which is how a test sees that a task was aborted.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A `Dropped` that takes a moment to finish. A scope that returns without
/// draining its tasks gets ahead of the flag. The delay orders nothing, so it
/// can't fail a scope that drains.
struct DroppedSlowly(Arc<AtomicBool>);

impl Drop for DroppedSlowly {
    fn drop(&mut self) {
        #[allow(
            clippy::disallowed_methods,
            reason = "a task being dropped can't await"
        )]
        std::thread::sleep(Duration::from_millis(20));
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A `DroppedSlowly` for several tasks: counts them once each has finished.
struct CountedSlowly(Arc<AtomicUsize>);

impl Drop for CountedSlowly {
    fn drop(&mut self) {
        #[allow(
            clippy::disallowed_methods,
            reason = "a task being dropped can't await"
        )]
        std::thread::sleep(Duration::from_millis(20));
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts itself live for as long as it exists, and keeps the highest count.
struct Live {
    live: Arc<AtomicUsize>,
}

impl Live {
    fn new(live: &Arc<AtomicUsize>, peak: &AtomicUsize) -> Self {
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        Self {
            live: Arc::clone(live),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A task that never finishes on its own and reports when it is dropped.
fn forever(
    dropped: &Arc<AtomicBool>,
) -> impl Future<Output = Result<(), &'static str>> + Send + use<> {
    let guard = Dropped(Arc::clone(dropped));
    async move {
        let _guard = guard;
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Polls `future` once and checks that it is waiting, so what the test does
/// next happens while it is under way.
async fn begin<F: Future + Send>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "the future finished before the test could act"
        );
        Poll::Ready(())
    })
    .await;
}

#[allow(clippy::unwrap_used, reason = "only ever called from tests")]
fn one_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Writes each test twice, once per runtime flavor.
macro_rules! on_both_runtimes {
    ($(async fn $name:ident() $body:block)*) => {$(
        mod $name {
            use super::*;

            #[tokio::test]
            async fn current_thread() $body

            #[tokio::test(flavor = "multi_thread")]
            async fn multi_thread() $body
        }
    )*};
}

on_both_runtimes! {
    async fn an_error_dropped_during_join_is_reported() {
        let mut tasks = Tasks::<&'static str>::new();
        let (sent, received) = oneshot::channel();
        let failed = tasks.spawn(async move {
            sent.send(()).unwrap();
            Err::<(), _>("dropped during join")
        });
        tasks.spawn(async move {
            received.await.unwrap();
            drop(failed);
            Ok(())
        });
        assert_eq!(tasks.join(Ok(())).await, Err("dropped during join"));
    }

    async fn a_sibling_that_drops_a_failed_handle_and_keeps_running_is_aborted() {
        let mut tasks = Tasks::<&str>::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let (sent, received) = oneshot::channel();
        let failed = tasks.spawn(async move {
            sent.send(()).unwrap();
            Err::<(), _>("sibling keeps running")
        });
        tasks.spawn(async move {
            let _guard = guard;
            received.await.unwrap();
            drop(failed);
            std::future::pending::<()>().await;
            Ok(())
        });
        let result = timeout(HUNG, tasks.join(Ok(()))).await.unwrap();
        assert_eq!(result, Err("sibling keeps running"));
        assert!(dropped.load(Ordering::SeqCst), "the sibling was not drained");
    }

    async fn a_sibling_still_holding_a_failed_handle_is_aborted() {
        let mut tasks = Tasks::<&str>::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let failed = tasks.spawn(async { Err::<(), _>("still held") });
        tasks.spawn(async move {
            let _guard = guard;
            let _failed = failed;
            std::future::pending::<()>().await;
            Ok(())
        });
        let result = timeout(HUNG, tasks.join(Ok(()))).await.unwrap();
        assert_eq!(result, Err("still held"));
        assert!(dropped.load(Ordering::SeqCst), "the sibling was not drained");
    }

    async fn a_handle_held_past_join_does_not_hide_its_error() {
        let mut tasks = Tasks::<&str>::new();
        let failed = tasks.spawn(async { Err::<(), _>("held past join") });
        assert_eq!(tasks.join(Ok(())).await, Err("held past join"));
        drop(failed);
    }

    async fn a_failure_during_join_aborts_and_drains_siblings() {
        let mut tasks = Tasks::<&str>::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DroppedSlowly(Arc::clone(&dropped));
        let (release, released) = oneshot::channel::<()>();
        tasks.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Ok(())
        });
        let held = tasks.spawn(async move {
            released.await.unwrap();
            Err::<(), _>("during join")
        });
        let mut join = pin!(timeout(HUNG, tasks.join(Ok(()))));
        begin(join.as_mut()).await;
        release.send(()).unwrap();
        assert_eq!(join.await.unwrap(), Err("during join"));
        assert!(dropped.load(Ordering::SeqCst), "the sibling was not drained");
        drop(held);
    }

    async fn an_error_the_body_took_is_not_reported_again() {
        let mut tasks = Tasks::<&str>::new();
        let failed = tasks.spawn(async { Err::<(), _>("handled") });
        assert_eq!(tasks.value(failed).await, Err("handled"));
        assert_eq!(tasks.join(Ok("recovered")).await, Ok("recovered"));
    }

    async fn value_takes_its_own_error_and_leaves_the_rest_for_join() {
        let mut tasks = Tasks::<&str>::new();
        let (sent, received) = oneshot::channel();
        tasks.spawn(async move {
            sent.send(()).unwrap();
            Err::<(), _>("left for join")
        });
        let asked_for = tasks.spawn(async move {
            received.await.unwrap();
            Err::<(), _>("asked for")
        });
        assert_eq!(tasks.value(asked_for).await, Err("asked for"));
        assert_eq!(tasks.join(Ok(())).await, Err("left for join"));
    }

    async fn an_abandoned_wait_for_a_value_leaves_the_error_for_join() {
        let mut tasks = Tasks::<&str>::new();
        let (release, released) = oneshot::channel::<()>();
        let failed = tasks.spawn(async move {
            released.await.unwrap();
            Err::<(), _>("nobody waited")
        });
        {
            let mut wait = pin!(tasks.value(failed));
            begin(wait.as_mut()).await;
        }
        release.send(()).unwrap();
        assert_eq!(tasks.join(Ok(())).await, Err("nobody waited"));
    }

    async fn a_body_error_wins_over_a_recorded_task_error() {
        let mut tasks = Tasks::<&str>::new();
        let (sent, received) = oneshot::channel();
        // Nothing can abort this task once it has signalled: it finishes in
        // the same poll, so its error is recorded before join returns.
        tasks.spawn(async move {
            sent.send(()).unwrap();
            Err::<(), _>("task")
        });
        received.await.unwrap();
        assert_eq!(tasks.join(Err::<(), _>("body")).await, Err("body"));
    }

    async fn a_panic_during_join_wins_over_an_error() {
        let mut tasks = Tasks::<&str>::new();
        let (sent, received) = oneshot::channel();
        // Nothing can abort this task once it has signalled: it panics in the
        // same poll, so the error below cannot cancel the panic.
        tasks.spawn::<(), _>(async move {
            sent.send(()).unwrap();
            panic!("kaboom after an error")
        });
        tasks.spawn(async move {
            received.await.unwrap();
            Err::<(), _>("error")
        });
        #[allow(clippy::disallowed_methods, reason = "the way to catch the resumed panic")]
        let joined = tokio::spawn(tasks.join(Ok(())));
        let payload = joined.await.unwrap_err().into_panic();
        assert_eq!(payload.downcast_ref(), Some(&"kaboom after an error"));
    }

    async fn a_panic_during_join_aborts_and_drains_siblings() {
        let mut tasks = Tasks::<&str>::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DroppedSlowly(Arc::clone(&dropped));
        let (started, sibling_started) = oneshot::channel();
        tasks.spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
            Ok(())
        });
        tasks.spawn::<(), _>(async move {
            sibling_started.await.unwrap();
            panic!("kaboom alone")
        });
        #[allow(clippy::disallowed_methods, reason = "the way to catch the resumed panic")]
        let joined = tokio::spawn(tasks.join(Ok(())));
        let payload = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
        assert_eq!(payload.downcast_ref(), Some(&"kaboom alone"));
        assert!(dropped.load(Ordering::SeqCst), "the sibling was not drained");
    }

    async fn concurrently_reports_a_later_error_while_an_earlier_item_is_pending() {
        let dropped = Arc::new(AtomicBool::new(false));
        let work = concurrently(0..2, 2, |n| {
            let guard = (n == 0).then(|| DroppedSlowly(Arc::clone(&dropped)));
            async move {
                let _guard = guard;
                if n == 0 {
                    std::future::pending::<()>().await;
                }
                Err::<(), _>("boom")
            }
        });
        assert_eq!(timeout(HUNG, work).await.unwrap(), Err("boom"));
        assert!(dropped.load(Ordering::SeqCst), "the pending item was not drained");
    }

    async fn concurrently_resumes_a_later_panic_while_an_earlier_item_is_pending() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let work = concurrently(0..2, 2, move |n| {
            let guard = (n == 0).then(|| DroppedSlowly(Arc::clone(&flag)));
            async move {
                let _guard = guard;
                assert!(n == 0, "kaboom behind a pending item");
                std::future::pending::<()>().await;
                Ok::<_, &str>(())
            }
        });
        #[allow(clippy::disallowed_methods, reason = "the way to catch the resumed panic")]
        let joined = tokio::spawn(work);
        let payload = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
        assert_eq!(payload.downcast_ref(), Some(&"kaboom behind a pending item"));
        assert!(dropped.load(Ordering::SeqCst), "the pending item was not drained");
    }

    // Which item does what depends on the order they start in, not on their
    // input position. The window is full when the failure happens, so the third
    // item is never taken.
    async fn concurrently_drains_running_items_and_starts_no_more_after_an_error() {
        let called = AtomicUsize::new(0);
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let work = concurrently(0..3, 2, |_| {
            called.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&started);
            let guard = CountedSlowly(Arc::clone(&dropped));
            async move {
                let _guard = guard;
                if started.fetch_add(1, Ordering::SeqCst) == 1 {
                    return Err("second to start");
                }
                std::future::pending::<()>().await;
                Ok(())
            }
        });
        assert_eq!(timeout(HUNG, work).await.unwrap(), Err("second to start"));
        assert_eq!(called.load(Ordering::SeqCst), 2, "an item started after the error");
        assert_eq!(dropped.load(Ordering::SeqCst), 2, "an item was not drained");
    }

    async fn concurrently_keeps_at_most_limit_futures_alive() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = AtomicUsize::new(0);
        let work = concurrently(0..1000, 2, |n| {
            let guard = Live::new(&live, &peak);
            async move {
                let _guard = guard;
                tokio::task::yield_now().await;
                Ok::<_, &str>(n)
            }
        });
        assert_eq!(work.await, Ok((0..1000).collect()));
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    async fn concurrently_takes_items_only_as_places_open() {
        let gate = Arc::new(Semaphore::new(0));
        let taken = AtomicUsize::new(0);
        let items = (0..5).inspect(|_| {
            taken.fetch_add(1, Ordering::SeqCst);
        });
        let mut work = pin!(concurrently(items, 2, |n| {
            let gate = Arc::clone(&gate);
            async move {
                gate.acquire().await.unwrap().forget();
                Ok::<_, &str>(n)
            }
        }));
        begin(work.as_mut()).await;
        assert_eq!(taken.load(Ordering::SeqCst), 2);
        gate.add_permits(5);
        assert_eq!(timeout(HUNG, work).await.unwrap(), Ok((0..5).collect()));
        assert_eq!(taken.load(Ordering::SeqCst), 5);
    }

    // The input never ends, so only stopping at the error can finish this. With
    // one place in the window, the failed item is the last one taken.
    async fn concurrently_takes_no_more_items_after_an_error() {
        let taken = AtomicUsize::new(0);
        let items = (0..).inspect(|_| {
            taken.fetch_add(1, Ordering::SeqCst);
        });
        let work = concurrently(items, 1, |n| async move {
            if n == 3 { Err("three") } else { Ok(n) }
        });
        assert_eq!(timeout(HUNG, work).await.unwrap(), Err("three"));
        assert_eq!(taken.load(Ordering::SeqCst), 4);
    }

    // This iterator would resume after its `None`, and the window still has an
    // item to wait for when it returns that `None`.
    async fn concurrently_takes_nothing_after_the_first_none() {
        let calls = AtomicUsize::new(0);
        let items = std::iter::from_fn(|| match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Some(0),
            1 => None,
            2 => Some(2),
            _ => panic!("called again after the end of the input"),
        });
        let work = concurrently(items, 2, |n| async move { Ok::<_, &str>(n) });
        assert_eq!(timeout(HUNG, work).await.unwrap(), Ok(vec![0]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // The first item waits for the last, which can only start if the window
    // moves past the first item while it is pending.
    async fn concurrently_rolls_past_a_slow_item_and_keeps_input_order() {
        let (release, released) = oneshot::channel();
        let mut release = Some(release);
        let mut released = Some(released);
        let work = concurrently(0..100, 2, |n| {
            let wait = (n == 0).then(|| released.take().unwrap());
            let release = (n == 99).then(|| release.take().unwrap());
            async move {
                if let Some(wait) = wait {
                    wait.await.unwrap();
                }
                if let Some(release) = release {
                    release.send(()).unwrap();
                }
                Ok::<_, &str>(n)
            }
        });
        assert_eq!(timeout(HUNG, work).await.unwrap(), Ok((0..100).collect()));
    }

    async fn concurrently_aborts_its_items_if_f_panics() {
        let (held, dropped) = oneshot::channel::<()>();
        let mut held = Some(held);
        let work = concurrently(0..2, 2, move |n| {
            assert!(n == 0, "kaboom in f");
            let held = held.take();
            async move {
                let _held = held;
                std::future::pending::<()>().await;
                Ok::<_, &str>(())
            }
        });
        #[allow(clippy::disallowed_methods, reason = "the way to catch the resumed panic")]
        let joined = tokio::spawn(work);
        let payload = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
        assert_eq!(payload.downcast_ref(), Some(&"kaboom in f"));
        assert!(timeout(HUNG, dropped).await.unwrap().is_err());
    }

    async fn concurrently_keeps_input_order_when_items_finish_in_reverse() {
        // Each item waits for the one after it, so the last finishes first.
        let mut release_previous = None;
        let mut items = Vec::new();
        for n in 0..8 {
            let (release, released) = oneshot::channel();
            items.push((n, released, release_previous.replace(release)));
        }
        release_previous.unwrap().send(()).unwrap();
        let work = concurrently(items, 8, |(n, released, release_earlier)| async move {
            released.await.unwrap();
            if let Some(release) = release_earlier {
                release.send(()).unwrap();
            }
            Ok::<_, &str>(n)
        });
        let result = timeout(HUNG, work).await.unwrap();
        assert_eq!(result, Ok((0..8).collect()));
    }

    async fn dropping_concurrently_cancels_its_items() {
        let (held, dropped) = oneshot::channel::<()>();
        let mut held = Some(held);
        {
            let mut work = pin!(concurrently(0..2, 2, |_| {
                let held = held.take();
                async move {
                    let _held = held;
                    std::future::pending::<()>().await;
                    Ok::<_, &str>(())
                }
            }));
            begin(work.as_mut()).await;
        }
        assert!(timeout(HUNG, dropped).await.unwrap().is_err());
    }
}

// One thread at a time, so the first task's error is recorded before the second
// task can run. A multithreaded runtime gives no such order.
#[tokio::test]
async fn the_first_error_recorded_wins_whatever_the_spawn_order() {
    let mut tasks = Tasks::<&str>::new();
    let (first_failing, second_may_fail) = oneshot::channel();
    let (second_failing, both_failing) = oneshot::channel();
    tasks.spawn(async move {
        second_may_fail.await.unwrap();
        second_failing.send(()).unwrap();
        Err::<(), _>("failed second")
    });
    tasks.spawn(async move {
        first_failing.send(()).unwrap();
        Err::<(), _>("failed first")
    });
    both_failing.await.unwrap();
    assert_eq!(tasks.join(Ok(())).await, Err("failed first"));
}

// A handle dropped just as its task delivers a result must not take the error
// with it. A barrier lines the two up, but the window is too small to hit every
// round, so this leans on repetition.
#[tokio::test(flavor = "multi_thread")]
async fn a_handle_dropped_as_its_task_fails_does_not_lose_the_error() {
    for _ in 0..10_000 {
        let mut tasks = Tasks::<&str>::new();
        let barrier = Arc::new(Barrier::new(2));
        let at_barrier = Arc::clone(&barrier);
        let failed = tasks.spawn(async move {
            at_barrier.wait().await;
            Err::<(), _>("raced")
        });
        barrier.wait().await;
        drop(failed);
        assert_eq!(tasks.join(Ok(())).await, Err("raced"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn values_come_back_in_the_order_asked() {
    let result = Tasks::<&str>::run(async |t| {
        let slow = t.spawn(async {
            sleep(Duration::from_millis(20)).await;
            Ok("slow")
        });
        let fast = t.spawn(async { Ok("fast") });
        Ok((t.value(slow).await?, t.value(fast).await?))
    })
    .await;
    assert_eq!(result, Ok(("slow", "fast")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_error_reaches_the_body_and_aborts_siblings() {
    let dropped = Arc::new(AtomicBool::new(false));
    let result = Tasks::<&str>::run(async |t| {
        t.spawn(forever(&dropped));
        let failing = t.spawn(async { Err::<(), _>("boom") });
        t.value(failing).await?;
        Ok("unreachable")
    })
    .await;
    assert_eq!(result, Err("boom"));
    assert!(
        dropped.load(Ordering::SeqCst),
        "the sibling was not aborted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_nobody_asked_for_is_reported_at_join() {
    let result = Tasks::<&str>::run(async |t| {
        t.spawn(async { Err::<(), _>("dropped handle") });
        Ok(())
    })
    .await;
    assert_eq!(result, Err("dropped handle"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_in_a_finished_task_whose_handle_is_dropped_later_is_reported() {
    let result = Tasks::<&str>::run(async |t| {
        let handle = t.spawn(async { Err::<(), _>("finished then dropped") });
        sleep(Duration::from_millis(20)).await;
        drop(handle);
        Ok(())
    })
    .await;
    assert_eq!(result, Err("finished then dropped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_error_wins_and_aborts_tasks() {
    let dropped = Arc::new(AtomicBool::new(false));
    let result = Tasks::<&str>::run(async |t| {
        t.spawn(forever(&dropped));
        Err::<(), _>("body")
    })
    .await;
    assert_eq!(result, Err("body"));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_scope_waits_for_every_task() {
    let finished = Arc::new(AtomicUsize::new(0));
    let result = Tasks::<&str>::run(async |t| {
        for _ in 0..8 {
            let finished = Arc::clone(&finished);
            t.spawn(async move {
                sleep(Duration::from_millis(5)).await;
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(finished.load(Ordering::SeqCst), 8);
}

#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "kaboom")]
async fn a_panic_resumes_where_the_value_is_asked_for() {
    let _ = Tasks::<&str>::run(async |t| {
        let handle = t.spawn::<(), _>(async { panic!("kaboom") });
        t.value(handle).await
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "kaboom at join")]
async fn a_panic_in_an_unwatched_task_resumes_at_join() {
    let _ = Tasks::<&str>::run(async |t| {
        t.spawn::<(), _>(async { panic!("kaboom at join") });
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manual_form_works_too() {
    let mut tasks = Tasks::<String>::new();
    let a = tasks.spawn(async { Ok(1) });
    let b = tasks.spawn(async { Ok(2) });
    assert_eq!(tasks.len(), 2);
    let body = async { Ok(tasks.value(a).await? + tasks.value(b).await?) }.await;
    assert_eq!(tasks.join(body).await, Ok(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrently_keeps_input_order_and_honors_the_limit() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let result = concurrently(0..20u64, 3, |n| {
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        async move {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            sleep(Duration::from_millis(5)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok::<_, &str>(n * 2)
        }
    })
    .await;
    assert_eq!(result, Ok((0..20).map(|n| n * 2).collect()));
    assert!(peak.load(Ordering::SeqCst) <= 3, "peak was {peak:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrently_stops_at_the_first_error() {
    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let result = concurrently(0..3, 8, |n| {
        let dropped = Arc::clone(&dropped);
        let started = Arc::clone(&started);
        async move {
            if n == 1 {
                // The sibling must own its guard before an error can cancel it.
                while !started.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                return Err("one");
            }
            if n == 2 {
                let _guard = Dropped(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            Ok(n)
        }
    })
    .await;
    assert_eq!(result, Err("one"));
    assert!(dropped.load(Ordering::SeqCst));
}

// One thread at a time, so the later item's error is recorded before the
// earlier item can run. A multithreaded runtime gives no such order.
#[tokio::test]
async fn concurrently_reports_the_first_error_to_happen_not_the_first_input() {
    let (failing, earlier_may_fail) = oneshot::channel();
    let mut failing = Some(failing);
    let mut earlier_may_fail = Some(earlier_may_fail);
    let work = concurrently(0..2, 2, |n| {
        let failing = (n == 1).then(|| failing.take().unwrap());
        let earlier_may_fail = (n == 0).then(|| earlier_may_fail.take().unwrap());
        async move {
            if let Some(earlier_may_fail) = earlier_may_fail {
                earlier_may_fail.await.unwrap();
                return Err::<(), _>("earlier input");
            }
            failing.unwrap().send(()).unwrap();
            Err("later input")
        }
    });
    assert_eq!(timeout(HUNG, work).await.unwrap(), Err("later input"));
}

// The future is first polled on the first runtime, so its task lives there and
// shutting it down cancels the task with nobody having aborted it and no error
// to report. The second item must not start on the runtime that is left.
#[test]
fn concurrently_panics_if_the_runtime_cancels_its_items() {
    let called = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&called);
    let mut work = Box::pin(concurrently(0..2, 1, move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async {
            std::future::pending::<()>().await;
            Ok::<_, &str>(())
        }
    }));
    let spawning = one_thread_runtime();
    spawning.block_on(begin(work.as_mut()));
    drop(spawning);
    #[allow(
        clippy::disallowed_methods,
        reason = "the way to catch the resumed panic"
    )]
    let joined = one_thread_runtime().block_on(async { timeout(HUNG, tokio::spawn(work)).await });
    let payload = joined.unwrap().unwrap_err().into_panic();
    assert_eq!(
        payload.downcast_ref(),
        Some(&"adamantium: a task ended without a result and without a panic"),
    );
    assert_eq!(called.load(Ordering::SeqCst), 1);
}

// The first item is stuck on a runtime that shuts down. The second finishes
// there, so the third starts on the runtime that is left, and only an abort can
// end it.
#[test]
fn concurrently_aborts_its_other_items_if_the_runtime_cancels_one() {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let (held, dropped) = oneshot::channel::<()>();
    let mut held = Some(held);
    let mut work = Box::pin(concurrently(0..3, 2, move |n| {
        let flag = Arc::clone(&flag);
        let held = (n == 2).then(|| held.take().unwrap());
        async move {
            let _held = held;
            if n == 1 {
                flag.store(true, Ordering::SeqCst);
                return Ok::<_, &str>(());
            }
            std::future::pending::<()>().await;
            Ok(())
        }
    }));
    let spawning = one_thread_runtime();
    spawning.block_on(async {
        begin(work.as_mut()).await;
        while !finished.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    });
    drop(spawning);
    #[allow(
        clippy::disallowed_methods,
        reason = "the way to catch the resumed panic"
    )]
    let joined = one_thread_runtime().block_on(async { timeout(HUNG, tokio::spawn(work)).await });
    let payload = joined.unwrap().unwrap_err().into_panic();
    assert_eq!(
        payload.downcast_ref(),
        Some(&"adamantium: a task ended without a result and without a panic"),
    );
    assert!(
        dropped.blocking_recv().is_err(),
        "the third item is still held"
    );
}

#[tokio::test]
async fn concurrently_treats_a_limit_of_zero_as_one() {
    let work = concurrently(0..3, 0, |n| async move { Ok::<_, &str>(n) });
    assert_eq!(timeout(HUNG, work).await.unwrap(), Ok(vec![0, 1, 2]));
}

#[tokio::test]
async fn concurrently_returns_a_vector_with_no_spare_room() {
    let values = concurrently(0..1000u64, 8, |n| async move { Ok::<_, &str>(n) })
        .await
        .unwrap();
    assert_eq!(values.capacity(), values.len());
}

#[test]
fn concurrently_does_nothing_until_polled() {
    let taken = AtomicUsize::new(0);
    let called = AtomicUsize::new(0);
    let items = (0..2).inspect(|_| {
        taken.fetch_add(1, Ordering::SeqCst);
    });
    let work = concurrently(items, 2, |_| {
        called.fetch_add(1, Ordering::SeqCst);
        async { Ok::<_, &str>(()) }
    });
    assert_eq!(taken.load(Ordering::SeqCst), 0);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    drop(work);
    assert_eq!(taken.load(Ordering::SeqCst), 0);
    assert_eq!(called.load(Ordering::SeqCst), 0);
}

// Calling needs no runtime, so the panic can only come from the poll.
#[test]
#[should_panic(expected = "Tokio 1.x runtime")]
fn concurrently_needs_a_runtime_once_polled() {
    let work = pin!(concurrently(0..1, 1, |_| async { Ok::<_, &str>(()) }));
    let _ = work.poll(&mut Context::from_waker(Waker::noop()));
}

// Each item goes straight into `f`, so only the iterator has to be `Send`.
#[tokio::test]
async fn concurrently_takes_items_that_are_not_send() {
    #[allow(clippy::disallowed_types, reason = "the item type under test")]
    let items = (0..2).map(std::rc::Rc::new);
    let work = concurrently(items, 2, |n| {
        let n = *n;
        async move { Ok::<_, &str>(n) }
    });
    assert_eq!(work.await, Ok(vec![0, 1]));
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_returns_the_closure_result() {
    assert_eq!(blocking(|| 6 * 7).await, 42);
}

#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "blocked and broke")]
async fn blocking_resumes_a_panic() {
    blocking(|| panic!("blocked and broke")).await;
}
