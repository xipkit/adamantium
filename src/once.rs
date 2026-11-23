use std::{
    convert::Infallible,
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::{Arc, OnceLock},
};

use crate::runtime::{
    Guard, Lock, World,
    polling::{Polling, polling_here},
    wait::WaitQueue,
};
#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, tasks::shutdown};
#[cfg(feature = "tokio")]
use std::task::Poll;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// A value that async work initializes once and then keeps.
///
/// [`Self::get_or_init`] returns the value, or runs an initializer to make
/// it. One initializer runs at a time. Calls that arrive while it runs wait
/// for it and get the same value. If the initializer fails, panics, or its
/// call is dropped, the cell stays empty and one waiting call runs its own
/// initializer. Clones share the cell.
///
/// ```
/// use adamantium::Once;
/// # async fn connect() -> Result<String, String> { Ok("connection".into()) }
///
/// # async fn example() -> Result<(), String> {
/// let database = Once::new();
/// // Concurrent first calls make one connection and share it.
/// let connection = database.get_or_try_init(connect).await?;
/// assert_eq!(connection, "connection");
/// # Ok(())
/// # }
/// ```
///
/// For a value that synchronous code makes, use `std::sync::OnceLock`.
///
/// # Panics
///
/// A call that the initializer makes for its own cell panics, because it
/// would wait for itself. An indirect cycle through a spawned task can still
/// deadlock.
pub struct Once<T> {
    shared: Arc<Shared<T>>,
}

struct Shared<T> {
    // Set once, under `initializing`, by the call that holds the claim.
    value: OnceLock<T>,
    // Whether a call is running its initializer.
    initializing: Lock<bool>,
    // Notified when the value is set or an initializer gives up.
    changed: WaitQueue,
    world: World,
}

impl<T> Once<T> {
    /// An empty cell.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                value: OnceLock::new(),
                initializing: Lock::new(false),
                changed: WaitQueue::named("once.wait"),
                world: World::current(),
            }),
        }
    }

    /// The value, or `None` if no initializer has finished. Never waits.
    pub fn get(&self) -> Option<&T> {
        self.shared.filled("Once::get")
    }

    /// Returns the value, running `init` to make it if the cell is empty.
    ///
    /// `init` is called only if the cell is still empty when no other call
    /// is running its initializer. Otherwise the call waits for that one.
    /// Dropping the future while `init` runs drops `init`'s future, leaves
    /// the cell empty, and lets a waiting call run its own initializer.
    ///
    /// # Panics
    ///
    /// Panics if the initializer of this cell makes this call itself, or if
    /// `init` panics.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the initializer and its future are"
    )]
    pub async fn get_or_init<F, Fut>(&self, init: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let Ok(value) = self
            .get_or_try_init(move || async move { Ok::<T, Infallible>(init().await) })
            .await;
        value
    }

    /// Returns the value, running `init` to make it if the cell is empty, as
    /// [`get_or_init`](Self::get_or_init) does.
    ///
    /// An error from `init` goes only to this call, and the cell stays empty.
    /// A waiting call then runs its own initializer.
    ///
    /// # Panics
    ///
    /// Panics if the initializer of this cell makes this call itself, or if
    /// `init` panics.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the initializer and its future are"
    )]
    pub async fn get_or_try_init<F, Fut, E>(&self, init: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(value) = self.shared.filled("Once::get_or_init") {
            return Ok(value);
        }
        let claim = match self.shared.changed.wait_for(|| self.claim()).await {
            Turn::Filled(value) => return Ok(value),
            Turn::Claimed(claim) => claim,
        };
        let value = self.initialize(init).await?;
        Ok(claim.fill(value))
    }

    // Takes the claim to initialize the cell, unless it is filled or another
    // call holds the claim.
    fn claim(&self) -> Option<Turn<'_, T>> {
        let mut initializing = self.shared.lock("Once::claim");
        if let Some(value) = self.shared.value.get() {
            return Some(Turn::Filled(value));
        }
        if *initializing {
            if polling_here(&self.shared) {
                drop(initializing);
                refuse_self_wait();
            }
            return None;
        }
        *initializing = true;
        Some(Turn::Claimed(Claim {
            shared: &self.shared,
            filled: false,
        }))
    }

    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the initializer and its future are"
    )]
    async fn initialize<Fut: Future>(&self, init: impl FnOnce() -> Fut) -> Fut::Output {
        // The closure itself can poll a nested call before returning its future.
        let mut future = pin!(async { init().await });
        poll_fn(|cx| {
            let _polling = Polling::enter(&self.shared);
            future.as_mut().poll(cx)
        })
        .await
    }
}

#[cfg(feature = "tokio")]
impl<T> Once<T> {
    /// Returns the value as [`get_or_try_init`](Self::get_or_try_init) does,
    /// until `cancellation` is requested or `deadline` passes. Build
    /// deadlines with [`crate::time`] or pass `None` for no deadline.
    ///
    /// Each poll checks cancellation and then the deadline before it takes
    /// the value or polls `init`. A request already present wins, even over
    /// a filled cell, so a cancelled call never calls `init`. A call that
    /// stops while `init` runs drops `init`'s future, and a waiting call runs
    /// its own initializer. Returns [`ScopeError::Failed`] with the error of
    /// `init`.
    ///
    /// # Panics
    ///
    /// Panics if the initializer of this cell makes this call itself, or if
    /// `init` panics.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the initializer and its future are"
    )]
    pub async fn get_or_try_init_until<F, Fut, E>(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        init: F,
    ) -> Result<&T, ScopeError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        // Polled by hand in `race` order: with `race` over async blocks that
        // return a reference, an async block awaiting this call can't be
        // proven `Send`, so it couldn't be spawned.
        let mut stop = pin!(shutdown(cancellation, deadline));
        let mut call = pin!(self.get_or_try_init(init));
        poll_fn(|cx| {
            if let Poll::Ready(reason) = stop.as_mut().poll(cx) {
                return Poll::Ready(Err(reason.scope_error()));
            }
            call.as_mut().poll(cx).map_err(ScopeError::Failed)
        })
        .await
    }
}

impl<T> Shared<T> {
    fn filled(&self, operation: &'static str) -> Option<&T> {
        self.world.step(operation);
        self.value.get()
    }

    // Every claim locks, so this is where a handle carried into another
    // harness execution is rejected. While unwinding the check is skipped,
    // so that a dropped call can still give up its claim.
    fn lock(&self, operation: &'static str) -> Guard<'_, bool> {
        self.world.check_unless_panicking();
        self.initializing.lock(operation)
    }
}

enum Turn<'a, T> {
    Filled(&'a T),
    Claimed(Claim<'a, T>),
}

// Held by the one call that runs its initializer.
struct Claim<'a, T> {
    shared: &'a Shared<T>,
    filled: bool,
}

impl<'a, T> Claim<'a, T> {
    fn fill(mut self, value: T) -> &'a T {
        let shared = self.shared;
        let mut initializing = shared.lock("Once::fill");
        // Only the holder of the claim sets the value, so this stores it.
        let value = shared.value.get_or_init(|| value);
        *initializing = false;
        drop(initializing);
        self.filled = true;
        shared.changed.notify_all();
        value
    }
}

impl<T> Drop for Claim<'_, T> {
    fn drop(&mut self) {
        if self.filled || self.shared.world.unwinding_in_foreign_world() {
            return;
        }
        *self.shared.lock("Once::release") = false;
        // One waiting call takes over. If it leaves before it runs, its
        // listener passes the notification to another waiting call.
        self.shared.changed.notify_one();
    }
}

const SELF_WAIT: &str = "adamantium: Once::get_or_init was called by the initializer of the same cell; \
it would wait for its own value, so call it from outside the initializer";

#[allow(
    clippy::panic,
    reason = "a call that waits for itself is a bug in the caller, and a panic names it where a deadlock would hang"
)]
fn refuse_self_wait() -> ! {
    std::panic::panic_any(SELF_WAIT)
}

impl<T> Clone for Once<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.shared.world.check();
        f.debug_struct("Once")
            .field("value", &self.shared.value.get())
            .finish()
    }
}
