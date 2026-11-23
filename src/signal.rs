use std::{fmt, sync::Arc};

#[cfg(feature = "tokio")]
use tokio::time::Instant;

use crate::runtime::{Guard, Lock, World, wait::WaitQueue};
#[cfg(feature = "tokio")]
use crate::{
    Cancellation, UntilError,
    tasks::{Shutdown, before_shutdown},
};

/// A wakeup that coalesces: a notification sent while another is pending
/// merges into it.
///
/// A `Signal` holds one flag, whether a notification is pending. Clones
/// share it. [`notify`](Self::notify) sets it and never waits.
/// [`notified`](Self::notified) waits until it is set, then clears it.
/// Nothing closes a signal.
///
/// Use it to tell a worker that state it reads, such as a queue in an
/// [`Actor`](crate::Actor), has changed. Any number of notifications can
/// arrive as one, so after each wakeup the worker should handle everything
/// that has arrived, not one item. When each notification must cause a
/// wakeup of its own, send on a [`Channel`](crate::Channel) instead.
///
/// # Delivery
///
/// - A notification stays pending until a call to `notified` takes it, and
///   only one call takes it: one that is waiting, or one that starts later.
///   A notification is never lost, even when it arrives as a wait begins.
/// - A `notify` that finds no notification pending wakes one waiting call.
///   One that finds a notification pending merges into it and wakes nobody,
///   so notifications sent while one is pending release one call, however
///   many are waiting.
/// - A woken call competes with any call that starts before it runs.
///   Whichever checks first takes the notification, and the other keeps
///   waiting. Waiting calls are served in no particular order.
/// - A call dropped before it returns takes nothing. If it was woken, the
///   wakeup passes to another waiting call, and with none, the notification
///   stays pending.
/// - A call that returns sees everything done before each `notify` that it
///   took or that merged into what it took.
///
/// # Compared with Tokio's `Notify`
///
/// - With no task waiting, `Notify::notify_one` stores one permit, as
///   `notify` does. With tasks waiting, it hands the permit to the task that
///   has waited longest, so a task that starts waiting later can't take it,
///   and `k` calls wake `k` tasks. `notify` hands the notification to no
///   waiter in particular, and `k` calls can wake one.
/// - `Notify::notify_waiters` wakes every task waiting at that moment and
///   stores nothing, so a task that starts waiting just after misses it.
///   `Signal` has no wake-all. Use a [`Watch`](crate::Watch) for a change
///   that every waiter must see, or a `Cancellation` for a one-time event
///   that stays set.
///
/// ```
/// # #[cfg(feature = "tokio")]
/// # #[tokio::main] async fn main() -> Result<(), &'static str> {
/// use adamantium::{Actor, Signal, Tasks};
///
/// let jobs = Actor::new(Vec::new());
/// let arrived = Signal::new();
/// let handled = Tasks::<&str>::run(async |t| {
///     let worker = t.spawn({
///         let (jobs, arrived) = (jobs.clone(), arrived.clone());
///         async move {
///             let mut handled = Vec::new();
///             while handled.len() < 3 {
///                 arrived.notified().await;
///                 // Take every job that has arrived, not just one.
///                 handled.extend(jobs.with(std::mem::take));
///             }
///             Ok(handled)
///         }
///     });
///     for job in 1..=3 {
///         jobs.with(|jobs| jobs.push(job));
///         arrived.notify();
///     }
///     t.value(worker).await
/// })
/// .await?;
/// assert_eq!(handled, [1, 2, 3]);
/// # Ok(()) }
/// # #[cfg(not(feature = "tokio"))]
/// # fn main() {}
/// ```
#[derive(Clone)]
pub struct Signal {
    shared: Arc<Shared>,
}

struct Shared {
    // Held only inside this module's synchronous methods, which run no
    // caller code.
    pending: Lock<bool>,
    world: World,
    notified: WaitQueue,
}

impl Shared {
    fn lock(&self, operation: &'static str) -> Guard<'_, bool> {
        self.world.check_unless_panicking();
        self.pending.lock(operation)
    }

    fn take(&self) -> Option<()> {
        std::mem::replace(&mut *self.lock("Signal::notified"), false).then_some(())
    }
}

impl Signal {
    /// A signal with no notification pending.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                pending: Lock::new(false),
                world: World::current(),
                notified: WaitQueue::named("signal.notified"),
            }),
        }
    }

    /// Makes a notification pending and returns without waiting. If none
    /// was pending, wakes one waiting [`notified`](Self::notified). If one
    /// was, this one merges into it and wakes nobody.
    pub fn notify(&self) {
        let was_pending = std::mem::replace(&mut *self.shared.lock("Signal::notify"), true);
        // A pending flag belongs to the call that set it, which wakes a
        // waiter itself. A wait that starts listening later checks the flag.
        if !was_pending {
            self.shared.notified.notify_one();
        }
    }

    /// Waits for a pending notification and takes it, returning at once if
    /// one is pending.
    ///
    /// Dropping the returned future loses nothing: a notification it hasn't
    /// taken stays pending, and a wakeup it received passes to another
    /// waiting call.
    pub async fn notified(&self) {
        self.shared.notified.wait_for(|| self.shared.take()).await;
    }

    /// Waits for a pending notification, as [`notified`](Self::notified)
    /// does, until `cancellation` is requested or the absolute `deadline`
    /// passes. Pass `None` for no deadline. A deadline needs Tokio's time
    /// driver.
    ///
    /// A request already present when the call is made, or at any later
    /// poll, wins over a pending notification, and cancellation wins over
    /// an equally ready deadline, as in [`Tasks::run_until`](crate::Tasks::run_until).
    /// Ending early leaves the notification pending.
    ///
    /// ```
    /// use adamantium::{Cancellation, Signal, UntilError, time};
    /// use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let signal = Signal::new();
    /// let cancellation = Cancellation::new();
    /// let soon = Some(time::now() + Duration::from_millis(10));
    /// assert_eq!(
    ///     signal.notified_until(&cancellation, soon).await,
    ///     Err(UntilError::DeadlineExceeded),
    /// );
    /// signal.notify();
    /// assert_eq!(signal.notified_until(&cancellation, None).await, Ok(()));
    /// # }
    /// ```
    #[cfg(feature = "tokio")]
    pub async fn notified_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<(), UntilError> {
        crate::runtime::in_world(self.shared.world, async move {
            before_shutdown(cancellation, deadline, self.notified())
                .await
                .map_err(Shutdown::until_error)
        })
        .await
    }
}

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pending = *self.shared.lock("Signal::fmt");
        f.debug_struct("Signal").field("pending", &pending).finish()
    }
}
