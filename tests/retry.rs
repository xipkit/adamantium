#![cfg(feature = "tokio")]

use adamantium::{Backoff, Cancellation, ScopeError, retry, retry_if};
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

/// Sets its flag when dropped, to show an attempt's future was dropped.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn waits_grow_exponentially_up_to_the_largest_delay() {
    let start = Instant::now();
    let mut starts = Vec::new();
    let result = retry(
        &Cancellation::new(),
        None,
        Backoff::exponential(secs(1), secs(4)),
        || {
            starts.push(start.elapsed());
            let result = if starts.len() < 6 { Err("no") } else { Ok(7) };
            async move { result }
        },
    )
    .await;
    assert_eq!(result, Ok(7));
    assert_eq!(
        starts,
        [secs(0), secs(1), secs(3), secs(7), secs(11), secs(15)]
    );
}

#[tokio::test(start_paused = true)]
async fn constant_backoff_waits_the_same_each_time() {
    let start = Instant::now();
    let mut starts = Vec::new();
    let result = retry(
        &Cancellation::new(),
        None,
        Backoff::constant(secs(2)).attempts(3),
        || {
            starts.push(start.elapsed());
            async { Err::<(), _>("no") }
        },
    )
    .await;
    assert_eq!(result, Err(ScopeError::Failed("no")));
    assert_eq!(starts, [secs(0), secs(2), secs(4)]);
}

#[tokio::test(start_paused = true)]
async fn returns_the_last_error_without_waiting_after_the_last_attempt() {
    let start = Instant::now();
    let mut calls = 0;
    let result = retry(
        &Cancellation::new(),
        None,
        Backoff::constant(HOUR).attempts(2),
        || {
            calls += 1;
            let error = calls;
            async move { Err::<(), _>(error) }
        },
    )
    .await;
    assert_eq!(result, Err(ScopeError::Failed(2)));
    assert_eq!(start.elapsed(), HOUR);
}

#[tokio::test(start_paused = true)]
async fn zero_attempts_makes_one() {
    let mut calls = 0;
    let result = retry(
        &Cancellation::new(),
        None,
        Backoff::constant(secs(1)).attempts(0),
        || {
            calls += 1;
            async { Err::<(), _>("no") }
        },
    )
    .await;
    assert_eq!(result, Err(ScopeError::Failed("no")));
    assert_eq!(calls, 1);
}

#[tokio::test(start_paused = true)]
async fn retry_if_stops_at_an_error_not_worth_retrying() {
    let start = Instant::now();
    let mut calls = 0;
    let result = retry_if(
        &Cancellation::new(),
        None,
        Backoff::constant(secs(1)),
        || {
            calls += 1;
            let error = if calls < 3 { "busy" } else { "denied" };
            async move { Err::<(), _>(error) }
        },
        |error| *error == "busy",
    )
    .await;
    assert_eq!(result, Err(ScopeError::Failed("denied")));
    assert_eq!(calls, 3);
    assert_eq!(start.elapsed(), secs(2));
}

#[tokio::test(start_paused = true)]
async fn huge_delays_saturate_instead_of_overflowing() {
    let mut calls = 0;
    let result = retry(
        &Cancellation::new(),
        None,
        Backoff::exponential(Duration::MAX, Duration::MAX).attempts(3),
        || {
            calls += 1;
            async { Err::<(), _>("no") }
        },
    )
    .await;
    assert_eq!(result, Err(ScopeError::Failed("no")));
    assert_eq!(calls, 3);
}

#[tokio::test(start_paused = true)]
async fn cancellation_ends_the_wait_between_attempts() {
    let cancellation = Cancellation::new();
    let start = Instant::now();
    let mut calls = 0;
    let (result, ()) = tokio::join!(
        retry(&cancellation, None, Backoff::constant(HOUR), || {
            calls += 1;
            async { Err::<(), _>("no") }
        }),
        cancel_at(&cancellation, start + secs(5)),
    );
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert_eq!(start.elapsed(), secs(5));
    assert_eq!(calls, 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_the_wait_between_attempts() {
    let start = Instant::now();
    let mut calls = 0;
    let result = retry(
        &Cancellation::new(),
        Some(start + secs(5)),
        Backoff::constant(HOUR),
        || {
            calls += 1;
            async { Err::<(), _>("no") }
        },
    )
    .await;
    assert_eq!(result, Err(ScopeError::DeadlineExceeded));
    assert_eq!(start.elapsed(), secs(5));
    assert_eq!(calls, 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_drops_an_attempt_in_flight() {
    let cancellation = Cancellation::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let (result, ()) = tokio::join!(
        retry(&cancellation, None, Backoff::constant(secs(1)), || {
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
async fn a_request_made_before_the_call_wins_over_the_first_attempt() {
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let mut calls = 0;
    let result = retry(&cancellation, None, Backoff::constant(HOUR), || {
        calls += 1;
        async { Ok::<_, ()>(()) }
    })
    .await;
    assert_eq!(result, Err(ScopeError::Cancelled));
    assert_eq!(calls, 0);
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_an_equally_ready_deadline() {
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let past = Instant::now() - secs(1);
    let result = retry(
        &cancellation,
        Some(past),
        Backoff::constant(HOUR),
        || async { Ok::<_, ()>(()) },
    )
    .await;
    assert_eq!(result, Err(ScopeError::Cancelled));
}
