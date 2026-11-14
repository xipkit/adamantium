//! The clock used for waits and deadlines, available with the `tokio` feature.
//!
//! Build deadlines from [`now`], for example
//! `time::now() + Duration::from_secs(5)`, and wait with [`sleep`] or
//! [`sleep_until`]. Deadlines remain [`tokio::time::Instant`] values accepted
//! by the library's `_until` methods. Use [`std::time::Instant::now`] to
//! measure elapsed wall-clock time instead.
//!
//! These functions currently delegate to Tokio and respect its paused clock
//! in `#[tokio::test(start_paused = true)]` tests.
//!
//! ```
//! use adamantium::time;
//! use std::time::Duration;
//! # #[tokio::main] async fn main() {
//! let deadline = time::now() + Duration::from_millis(10);
//! time::sleep_until(deadline).await;
//! time::sleep(Duration::from_millis(10)).await;
//! # }
//! ```

use std::time::Duration;

use tokio::time::Instant;

/// Returns the current instant for building deadlines.
#[allow(
    clippy::disallowed_methods,
    reason = "the clock entry point delegates to Tokio here"
)]
pub fn now() -> Instant {
    Instant::now()
}

/// Waits for `duration` to elapse without blocking the worker thread.
///
/// Uses Tokio's time driver, with millisecond timer granularity. Dropping the
/// future cancels the wait. Awaiting it requires a runtime with time enabled.
#[allow(
    clippy::disallowed_methods,
    reason = "the relative wait entry point delegates to Tokio here"
)]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Waits until `deadline`, built from [`now`], without blocking the worker thread.
///
/// Uses Tokio's time driver, with millisecond timer granularity. Dropping the
/// future cancels the wait. Awaiting it requires a runtime with time enabled.
#[allow(
    clippy::disallowed_methods,
    reason = "the deadline wait entry point delegates to Tokio here"
)]
pub async fn sleep_until(deadline: Instant) {
    tokio::time::sleep_until(deadline).await;
}
