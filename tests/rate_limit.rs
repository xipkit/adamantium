#![cfg(feature = "tokio")]

use adamantium::{Cancellation, RateLimit, ScopeError, concurrently};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::time::{Duration, Instant, sleep, sleep_until};

const HOUR: Duration = Duration::from_secs(3600);

fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// Requests shutdown at `at`, from a future polled alongside the combinator.
async fn cancel_at(cancellation: &Cancellation, at: Instant) {
    sleep_until(at).await;
    cancellation.cancel();
}

/// Runs a call under `limit` that does nothing but succeed.
async fn call(limit: &RateLimit, cancellation: &Cancellation) -> Result<(), ScopeError<()>> {
    limit
        .run_until(cancellation, None, || async { Ok(()) })
        .await
}

/// Sets its flag when dropped, to show the work's future was dropped.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn lets_a_burst_through_then_waits_for_the_window_to_move() {
    let limit = RateLimit::new(2, secs(10));
    let cancellation = Cancellation::new();
    let start = Instant::now();
    let mut starts = Vec::new();
    for _ in 0..5 {
        call(&limit, &cancellation).await.unwrap();
        starts.push(start.elapsed());
    }
    assert_eq!(starts, [secs(0), secs(0), secs(10), secs(10), secs(20)]);
}

#[tokio::test(start_paused = true)]
async fn an_idle_limit_allows_one_full_window_and_no_more() {
    let limit = RateLimit::new(3, secs(10));
    let cancellation = Cancellation::new();
    for _ in 0..3 {
        call(&limit, &cancellation).await.unwrap();
    }
    sleep(HOUR).await;
    let start = Instant::now();
    let mut starts = Vec::new();
    for _ in 0..4 {
        call(&limit, &cancellation).await.unwrap();
        starts.push(start.elapsed());
    }
    assert_eq!(starts, [secs(0), secs(0), secs(0), secs(10)]);
}

#[tokio::test(start_paused = true)]
async fn zero_permits_allows_one() {
    let limit = RateLimit::new(0, secs(10));
    let cancellation = Cancellation::new();
    let start = Instant::now();
    call(&limit, &cancellation).await.unwrap();
    call(&limit, &cancellation).await.unwrap();
    assert_eq!(start.elapsed(), secs(10));
}

#[tokio::test(start_paused = true)]
async fn clones_share_one_limit_across_concurrent_work() {
    let limit = RateLimit::new(2, secs(1));
    let cancellation = Cancellation::new();
    let start = Instant::now();
    let mut starts = concurrently(0..7, 7, |_| {
        let (limit, cancellation) = (limit.clone(), cancellation.clone());
        async move {
            limit
                .run_until(&cancellation, None, || async {
                    Ok::<_, ()>(start.elapsed())
                })
                .await
        }
    })
    .await
    .unwrap();
    starts.sort();
    assert_eq!(
        starts,
        [
            secs(0),
            secs(0),
            secs(1),
            secs(1),
            secs(2),
            secs(2),
            secs(3)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn returns_the_work_error_as_failed() {
    let limit = RateLimit::new(1, secs(1));
    let result = limit
        .run_until(&Cancellation::new(), None, || async { Err::<(), _>("no") })
        .await;
    assert_eq!(result, Err(ScopeError::Failed("no")));
}

#[tokio::test(start_paused = true)]
async fn cancellation_ends_the_wait_for_room() {
    let limit = RateLimit::new(1, HOUR);
    let cancellation = Cancellation::new();
    call(&limit, &cancellation).await.unwrap();
    let start = Instant::now();
    let mut ran = false;
    let (result, ()) = tokio::join!(
        limit.run_until(&cancellation, None, || {
            ran = true;
            async { Ok::<_, ()>(()) }
        }),
        cancel_at(&cancellation, start + secs(5)),
    );
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert_eq!(start.elapsed(), secs(5));
    assert!(!ran);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_the_wait_for_room() {
    let limit = RateLimit::new(1, HOUR);
    let cancellation = Cancellation::new();
    call(&limit, &cancellation).await.unwrap();
    let start = Instant::now();
    let result = limit
        .run_until(&cancellation, Some(start + secs(5)), || async {
            Ok::<_, ()>(())
        })
        .await;
    assert_eq!(result, Err(ScopeError::DeadlineExceeded));
    assert_eq!(start.elapsed(), secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_wait_past_the_end_of_time_still_ends_when_cancelled() {
    let limit = RateLimit::new(1, Duration::MAX);
    let cancellation = Cancellation::new();
    call(&limit, &cancellation).await.unwrap();
    let start = Instant::now();
    let (result, ()) = tokio::join!(
        call(&limit, &cancellation),
        cancel_at(&cancellation, start + secs(5)),
    );
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert_eq!(start.elapsed(), secs(5));
}

#[tokio::test(start_paused = true)]
async fn cancellation_drops_work_in_flight() {
    let limit = RateLimit::new(1, secs(1));
    let cancellation = Cancellation::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let (result, ()) = tokio::join!(
        limit.run_until(&cancellation, None, || {
            let guard = Dropped(Arc::clone(&dropped));
            async move {
                let _guard = guard;
                sleep(HOUR).await;
                Ok::<_, ()>(())
            }
        }),
        cancel_at(&cancellation, start + secs(5)),
    );
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert_eq!(start.elapsed(), secs(5));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_request_made_before_the_call_takes_no_room() {
    let limit = RateLimit::new(1, HOUR);
    let cancelled = Cancellation::new();
    cancelled.cancel();
    let mut ran = false;
    let result = limit
        .run_until(&cancelled, None, || {
            ran = true;
            async { Ok::<_, ()>(()) }
        })
        .await;
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert!(!ran);

    let start = Instant::now();
    call(&limit, &Cancellation::new()).await.unwrap();
    assert_eq!(start.elapsed(), Duration::ZERO);
}
