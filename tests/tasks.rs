#![cfg(feature = "tokio")]

use std::{
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

use adamantium::{Tasks, blocking, concurrently};
use tokio::{
    sync::{Barrier, oneshot},
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

#[tokio::test(flavor = "multi_thread")]
async fn blocking_returns_the_closure_result() {
    assert_eq!(blocking(|| 6 * 7).await, 42);
}

#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "blocked and broke")]
async fn blocking_resumes_a_panic() {
    blocking(|| panic!("blocked and broke")).await;
}
