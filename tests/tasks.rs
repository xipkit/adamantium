#![cfg(feature = "tokio")]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use adamantium::{Tasks, blocking, concurrently};
use tokio::time::sleep;

/// Sets a flag when dropped, which is how a test sees that a task was aborted.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
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
