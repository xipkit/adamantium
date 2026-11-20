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
/// Waits longer than 30 years are capped at 30 years, on every supported Tokio
/// version and in the deterministic harness.
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

/// The longest relative wait, matching the far-future delay newer Tokio
/// versions use. Older versions keep any representable delay, so capping here
/// gives every supported version and the modeled clock the same contract.
const MAX_DELAY: Duration = Duration::from_secs(86400 * 365 * 30);

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
    let delay = match delay {
        Delay::For(duration) => Delay::For(duration.min(MAX_DELAY)),
        Delay::Until(deadline) => Delay::Until(deadline),
    };
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

/// How [`Interval`] schedules the next tick when a tick completes at or after
/// the following scheduled deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// Schedules the next tick one `period` after the current time.
    ///
    /// Prevents burst catch-up after a slow iteration by spacing the next tick
    /// a full `period` from when the missed tick returned. This is the default.
    #[default]
    Delay,
    /// Advances by whole multiples of `period` to the next scheduled tick
    /// strictly after the current time.
    ///
    /// Preserves the original phase grid (`start + k * period`), skipping any
    /// ticks that fell in the past without firing a burst.
    Skip,
}

/// A drift-free periodic timer built on [`adamantium::time`](self).
///
/// Uses Tokio's clock in production (and its paused test clock in unit tests)
/// and virtual time inside `adamantium::testing`.
///
/// Unlike `tokio::time::interval`, `Interval` defaults to
/// [`MissedTickBehavior::Delay`] rather than burst catch-up, does not panic on a
/// zero period, and provides [`Interval::tick_until`] for clean cancellation.
///
/// ```
/// use adamantium::{Cancellation, time};
/// use std::time::Duration;
/// # #[tokio::main] async fn main() {
/// let shutdown = Cancellation::new();
/// let mut ticks = time::Interval::new(Duration::from_millis(5));
/// let first = ticks.tick_until(&shutdown).await;
/// assert!(first.is_some());
/// shutdown.cancel();
/// assert_eq!(ticks.tick_until(&shutdown).await, None);
/// # }
/// ```
#[derive(Debug)]
pub struct Interval {
    next: Instant,
    period: Duration,
    behavior: MissedTickBehavior,
}

impl Interval {
    /// Creates an interval that yields every `period`, with the first tick
    /// completing immediately.
    ///
    /// A zero `period` yields immediately on every tick without panicking.
    pub fn new(period: Duration) -> Self {
        Self::at(now(), period)
    }

    /// Creates an interval that yields at `start` and every `period`
    /// thereafter.
    ///
    /// A zero `period` waits until `start` and then yields immediately on every
    /// subsequent tick without panicking.
    pub fn at(start: Instant, period: Duration) -> Self {
        Self {
            next: start,
            period,
            behavior: MissedTickBehavior::default(),
        }
    }

    /// Sets the [`MissedTickBehavior`] for this interval.
    #[must_use]
    pub fn with_missed_tick_behavior(mut self, behavior: MissedTickBehavior) -> Self {
        self.behavior = behavior;
        self
    }

    /// Updates the [`MissedTickBehavior`] for this interval.
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }

    /// Returns the current [`MissedTickBehavior`].
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    /// Returns the interval's period.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Resets the interval so the next tick completes `period` after now.
    pub fn reset(&mut self) {
        self.next = add_capped(now(), self.period);
    }

    /// Resets the interval so the next tick completes at `deadline`.
    pub fn reset_at(&mut self, deadline: Instant) {
        self.next = deadline;
    }

    /// Waits until the next scheduled tick and returns the instant at which it
    /// completed.
    pub async fn tick(&mut self) -> Instant {
        sleep_until(self.next).await;
        let current = now();
        self.advance(current);
        current
    }

    /// Waits until the next scheduled tick or `cancellation`, whichever comes
    /// first.
    ///
    /// Returns `Some(instant)` when the tick fires, or `None` when
    /// `cancellation` is requested without advancing the schedule. As in
    /// [`Tasks::run_until`](crate::Tasks::run_until), a cancellation already
    /// present at a poll wins over an equally ready tick.
    pub async fn tick_until(&mut self, cancellation: &crate::Cancellation) -> Option<Instant> {
        match crate::tasks::shutdown(cancellation, Some(self.next)).await {
            crate::tasks::Shutdown::Cancelled => None,
            crate::tasks::Shutdown::DeadlineExceeded => {
                let current = now();
                self.advance(current);
                Some(current)
            }
        }
    }

    fn advance(&mut self, current: Instant) {
        if self.period.is_zero() {
            self.next = self.next.max(current);
            return;
        }
        let next = add_capped(self.next, self.period);
        if current < next {
            self.next = next;
            return;
        }
        self.next = match self.behavior {
            MissedTickBehavior::Delay => add_capped(current, self.period),
            MissedTickBehavior::Skip => {
                let late = current.saturating_duration_since(self.next);
                let period_nanos = self.period.as_nanos();
                let steps = late.as_nanos() / period_nanos + 1;
                let offset = steps
                    .checked_mul(period_nanos)
                    .map_or(MAX_DELAY, duration_from_nanos_capped);
                add_capped(self.next, offset)
            }
        };
    }
}

fn add_capped(base: Instant, offset: Duration) -> Instant {
    base.checked_add(offset)
        .or_else(|| base.checked_add(offset.min(MAX_DELAY)))
        .unwrap_or(base)
}

fn duration_from_nanos_capped(nanos: u128) -> Duration {
    let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
    if secs == u64::MAX {
        return Duration::MAX;
    }
    let subsec_nanos = u32::try_from(nanos % 1_000_000_000).unwrap_or(0);
    Duration::new(secs, subsec_nanos)
}
