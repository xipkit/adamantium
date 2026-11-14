use std::{future::IntoFuture, time::Duration};

/// Bounds a test wait so a deadlock fails the test instead of hanging it.
#[track_caller]
#[allow(
    clippy::disallowed_methods,
    reason = "test watchdog; expiry is a test failure, not application cancellation"
)]
pub fn timeout<F: IntoFuture>(
    duration: Duration,
    future: F,
) -> tokio::time::Timeout<F::IntoFuture> {
    tokio::time::timeout(duration, future)
}
