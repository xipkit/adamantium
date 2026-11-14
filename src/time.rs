//! The clock used for waits and deadlines, available with the `tokio` feature.
//!
//! Build deadlines from [`now`], for example
//! `time::now() + Duration::from_secs(5)`, and wait with [`sleep`] or
//! [`sleep_until`]. Deadlines remain [`tokio::time::Instant`] values accepted
//! by the library's `_until` methods. Use [`std::time::Instant::now`] to
//! measure elapsed wall-clock time instead.
//!
//! Outside `adamantium::testing` (with the `deterministic` feature), these functions
//! delegate to Tokio, including its paused test clock. Inside an execution they
//! use virtual time: the clock advances to the earliest pending deadline only
//! when no application task can run. All timers at that instant become ready
//! together; the scheduler explores the order in which their tasks resume.
//!
//! Each execution starts at the same opaque origin in this process. Compare
//! elapsed durations, not the origin, across processes. Build deadlines inside
//! the execution that uses them; `Instant` itself carries no execution identity.
//! A sleep belongs to the execution where it is first polled and cannot be
//! polled or dropped in another execution. Dropping it unregisters its timer.
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
    #[cfg(feature = "deterministic")]
    if crate::runtime::World::current().is_model() {
        return crate::runtime::clock::now();
    }
    Instant::now()
}

/// Waits for `duration` to elapse without blocking the worker thread.
///
/// Starts on the first poll. Uses virtual time in the deterministic harness;
/// otherwise requires Tokio's time driver, with millisecond timer granularity.
/// Dropping the future cancels the wait.
/// Very large relative waits use a finite far-future deadline, like Tokio.
pub async fn sleep(duration: Duration) {
    wait(Delay::For(duration)).await;
}

/// Waits until `deadline`, built from [`now`], without blocking the worker thread.
///
/// Uses virtual time in the deterministic harness; otherwise requires Tokio's
/// time driver, with millisecond timer granularity. Dropping the future cancels
/// the wait. Past and current virtual deadlines are immediately ready.
pub async fn sleep_until(deadline: Instant) {
    wait(Delay::Until(deadline)).await;
}

enum Delay {
    For(Duration),
    Until(Instant),
}

struct Owner(crate::runtime::World);

impl Drop for Owner {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.0.check();
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the clock boundary selects the production time driver"
)]
async fn wait(delay: Delay) {
    let owner = Owner(crate::runtime::World::current());
    crate::runtime::in_world(owner.0, async {
        #[cfg(feature = "deterministic")]
        if owner.0.is_model() {
            let deadline = match delay {
                Delay::For(duration) => crate::runtime::clock::deadline_after(duration),
                Delay::Until(deadline) => deadline,
            };
            crate::runtime::clock::Sleep::new(deadline).await;
            return;
        }
        match delay {
            Delay::For(duration) => tokio::time::sleep(duration).await,
            Delay::Until(deadline) => tokio::time::sleep_until(deadline).await,
        }
    })
    .await;
}
