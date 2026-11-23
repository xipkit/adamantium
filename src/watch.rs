use std::{fmt, sync::Arc};

use crate::runtime::wait::WaitQueue;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::ReceiveError;
#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, before_shutdown};
use crate::{
    Closed, TryReceiveError,
    runtime::{Guard, Lock, World},
};

/// The latest value of some state, such as configuration, that tasks can
/// read and wait to change.
///
/// A `Watch` holds one value. [`send`](Self::send) replaces it and wakes
/// every [`Watcher`]; watchers made with [`subscribe`](Self::subscribe) see
/// the newest value, not every value in between. Clones share the value and
/// can all send. Dropping the last `Watch`, or calling
/// [`close`](Self::close), closes it: watchers still read the last value,
/// and their waits end once they have seen it.
///
/// ```
/// # #[cfg(feature = "tokio")]
/// # #[tokio::main] async fn main() -> Result<(), &'static str> {
/// use adamantium::{Tasks, Watch};
///
/// let limit = Watch::new(10_u32);
/// let seen = Tasks::<&str>::run(async |t| {
///     let mut watcher = limit.subscribe();
///     let worker = t.spawn(async move {
///         let mut seen = vec![watcher.get()];
///         while let Some(limit) = watcher.receive().await {
///             seen.push(limit);
///         }
///         Ok(seen)
///     });
///     limit.send(20).map_err(|_| "closed")?;
///     limit.close();
///     t.value(worker).await
/// })
/// .await?;
/// // The worker may start before or after the change, but always ends on it.
/// assert!(seen == [10, 20] || seen == [20]);
/// # Ok(()) }
/// # #[cfg(not(feature = "tokio"))]
/// # fn main() {}
/// ```
pub struct Watch<T> {
    shared: Arc<Shared<T>>,
}

/// One task's view of a [`Watch`], made by [`Watch::subscribe`].
///
/// A watcher remembers which value it last saw. [`receive`](Self::receive)
/// waits for a newer one, and [`get`](Self::get) reads the current value
/// and marks it seen, so reading a value and then waiting for the next
/// never returns the same one twice. A clone starts having seen what the
/// original has seen, and from then on keeps its own place.
pub struct Watcher<T> {
    shared: Arc<Shared<T>>,
    seen: u64,
}

struct Shared<T> {
    // Held only inside this module's synchronous methods, which run no
    // caller code but `T::clone` and `T::fmt`.
    state: Lock<State<T>>,
    world: World,
    changed: WaitQueue,
}

struct State<T> {
    value: T,
    // A watcher has seen the value when its send count matches. Counters wrap.
    version: u64,
    closed: bool,
    watches: usize,
}

impl<T> Shared<T> {
    // Every operation locks, so this is where a handle carried into the
    // deterministic harness is rejected. While unwinding the check is
    // skipped: the execution is already failing, and a second panic would
    // abort the process.
    fn lock(&self, operation: &'static str) -> Guard<'_, State<T>> {
        self.world.check_unless_panicking();
        self.state.lock(operation)
    }

    fn close(&self) -> bool {
        self.close_with(self.lock("Watch::close"))
    }

    fn close_with(&self, mut state: Guard<'_, State<T>>) -> bool {
        let was_open = !std::mem::replace(&mut state.closed, true);
        drop(state);
        if was_open {
            self.changed.notify_all();
        }
        was_open
    }
}

impl<T> Watch<T> {
    /// A watch holding `value`.
    pub fn new(value: T) -> Self {
        Self {
            shared: Arc::new(Shared {
                world: World::current(),
                state: Lock::new(State {
                    value,
                    version: 0,
                    closed: false,
                    watches: 1,
                }),
                changed: WaitQueue::named("watch.receive"),
            }),
        }
    }

    /// Replaces the value and wakes every watcher waiting for a change.
    /// Hands the value back in [`Closed`] if the watch is closed.
    ///
    /// Sending needs no watchers: one subscribed later reads the new value.
    pub fn send(&self, value: T) -> Result<(), Closed<T>> {
        let old = {
            let mut state = self.shared.lock("Watch::send");
            if state.closed {
                return Err(Closed(value));
            }
            state.version = state.version.wrapping_add(1);
            std::mem::replace(&mut state.value, value)
        };
        self.shared.changed.notify_all();
        // Unlock and wake before dropping the old value. Its destructor may panic.
        drop(old);
        Ok(())
    }

    /// A watcher that has seen the current value, so its first
    /// [`receive`](Watcher::receive) waits for the next send.
    pub fn subscribe(&self) -> Watcher<T> {
        Watcher {
            shared: Arc::clone(&self.shared),
            seen: self.shared.lock("Watch::subscribe").version,
        }
    }

    /// Closes the watch for every handle and wakes waiting watchers, which
    /// return the last value if they haven't seen it and then `None`.
    /// Returns `false` if it was already closed.
    pub fn close(&self) -> bool {
        self.shared.close()
    }

    /// Whether the watch is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.lock("Watch::is_closed").closed
    }
}

impl<T: Clone> Watch<T> {
    /// A copy of the current value.
    pub fn get(&self) -> T {
        self.shared.lock("Watch::get").value.clone()
    }
}

impl<T> Watcher<T> {
    /// Whether a value this watcher hasn't seen is waiting.
    pub fn has_changed(&self) -> bool {
        self.shared.lock("Watcher::has_changed").version != self.seen
    }

    /// Whether the watch is closed. An unseen last value can still be
    /// received.
    pub fn is_closed(&self) -> bool {
        self.shared.lock("Watcher::is_closed").closed
    }

    /// Waits for a value this watcher hasn't seen and marks it seen, as
    /// [`receive`](Self::receive) does, without copying it. Returns `None`
    /// once the watch is closed and its last value has been seen. Read the
    /// value with [`get`](Self::get) where it is needed.
    ///
    /// Dropping the returned future loses nothing: the value stays unseen.
    pub async fn changed(&mut self) -> Option<()> {
        self.shared
            .world
            .cooperative(self.changed_unbudgeted())
            .await
    }

    pub(crate) async fn changed_unbudgeted(&mut self) -> Option<()> {
        loop {
            if let Some(changed) = self.mark() {
                return changed;
            }
            // Register before the second check so a concurrent send wakes the wait.
            let listener = self.shared.changed.listen();
            if let Some(changed) = self.mark() {
                return changed;
            }
            listener.await;
        }
    }

    fn mark(&mut self) -> Option<Option<()>> {
        let state = self.shared.lock("Watcher::changed");
        if state.version != self.seen {
            self.seen = state.version;
            Some(Some(()))
        } else if state.closed {
            Some(None)
        } else {
            None
        }
    }
}

impl<T: Clone> Watcher<T> {
    /// A copy of the current value, which this watcher then counts as
    /// seen. Use [`has_changed`](Self::has_changed) to look without marking.
    pub fn get(&mut self) -> T {
        let state = self.shared.lock("Watcher::get");
        let value = state.value.clone();
        self.seen = state.version;
        value
    }

    /// Waits for a value this watcher hasn't seen, marks it seen and
    /// returns a copy. Values sent in between are skipped. Returns `None`
    /// once the watch is closed and its last value has been seen.
    ///
    /// Dropping the returned future loses nothing: the value stays unseen.
    pub async fn receive(&mut self) -> Option<T> {
        self.shared.world.cooperative(self.receive_inner()).await
    }

    async fn receive_inner(&mut self) -> Option<T> {
        loop {
            if let Some(received) = self.check("Watcher::receive") {
                return received;
            }
            let listener = self.shared.changed.listen();
            if let Some(received) = self.check("Watcher::receive") {
                return received;
            }
            listener.await;
        }
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// this watcher has seen the current value and the watch is open, or
    /// [`TryReceiveError::Closed`] once it is closed and the last value has
    /// been seen.
    pub fn try_receive(&mut self) -> Result<T, TryReceiveError> {
        match self.check("Watcher::try_receive") {
            Some(Some(value)) => Ok(value),
            Some(None) => Err(TryReceiveError::Closed),
            None => Err(TryReceiveError::Empty),
        }
    }

    /// Waits until the value satisfies `condition` and returns a copy,
    /// checking the current value first, seen or not. Each value checked is
    /// marked seen. Returns `None` if the watch closes first.
    ///
    /// ```
    /// # #[cfg(feature = "tokio")]
    /// # #[tokio::main] async fn main() {
    /// use adamantium::Watch;
    ///
    /// let ready = Watch::new(false);
    /// let mut watcher = ready.subscribe();
    /// ready.send(true).unwrap();
    /// // The value changed before the wait began, and the wait still sees it.
    /// assert_eq!(watcher.wait_for(|ready| *ready).await, Some(true));
    /// # }
    /// # #[cfg(not(feature = "tokio"))]
    /// # fn main() {}
    /// ```
    pub async fn wait_for(&mut self, mut condition: impl FnMut(&T) -> bool) -> Option<T> {
        self.shared
            .world
            .cooperative(async {
                let mut value = self.get();
                loop {
                    if condition(&value) {
                        return Some(value);
                    }
                    value = self.receive_inner().await?;
                }
            })
            .await
    }

    /// Waits for a value this watcher hasn't seen, as in
    /// [`receive`](Self::receive), until `cancellation` is requested or the
    /// absolute `deadline` passes. Pass `None` for no deadline. A deadline
    /// needs Tokio's time driver.
    ///
    /// A request already present when the call is made, or at any later
    /// poll, wins over an unseen value, and cancellation wins over an
    /// equally ready deadline, as in [`Tasks::run_until`](crate::Tasks::run_until).
    /// Ending early leaves the value unseen.
    #[cfg(feature = "tokio")]
    pub async fn receive_until(
        &mut self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<T, ReceiveError> {
        crate::runtime::in_world(self.shared.world, async move {
            before_shutdown(cancellation, deadline, self.receive())
                .await
                .map_err(Shutdown::receive_error)?
                .ok_or(ReceiveError::Closed)
        })
        .await
    }

    fn check(&mut self, operation: &'static str) -> Option<Option<T>> {
        let state = self.shared.lock(operation);
        if state.version != self.seen {
            let value = state.value.clone();
            self.seen = state.version;
            Some(Some(value))
        } else if state.closed {
            Some(None)
        } else {
            None
        }
    }
}

impl<T> Clone for Watch<T> {
    fn clone(&self) -> Self {
        let mut state = self.shared.lock("Watch::clone");
        state.watches = state.watches.saturating_add(1);
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Watch<T> {
    fn drop(&mut self) {
        if self.shared.world.unwinding_in_foreign_world() {
            return;
        }
        let mut state = self.shared.lock("Watch::drop");
        state.watches = state.watches.saturating_sub(1);
        if state.watches == 0 {
            self.shared.close_with(state);
        }
    }
}

impl<T> Clone for Watcher<T> {
    fn clone(&self) -> Self {
        self.shared.world.step("Watcher::clone");
        Self {
            shared: Arc::clone(&self.shared),
            seen: self.seen,
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Watch<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_state(f, "Watch", &self.shared, None, "Watch::fmt")
    }
}

impl<T: fmt::Debug> fmt::Debug for Watcher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_state(f, "Watcher", &self.shared, Some(self.seen), "Watcher::fmt")
    }
}

fn debug_state<T: fmt::Debug>(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    shared: &Shared<T>,
    seen: Option<u64>,
    operation: &'static str,
) -> fmt::Result {
    // Try the lock to avoid deadlock when T::clone or T::fmt formats the watch.
    shared.world.check();
    let Some(state) = shared.state.try_lock(operation) else {
        return write!(f, "{name}(<in use>)");
    };
    let mut debug = f.debug_struct(name);
    debug.field("value", &state.value);
    if let Some(seen) = seen {
        debug.field("changed", &(state.version != seen));
    }
    debug.field("closed", &state.closed).finish()
}

impl<T> Drop for Watcher<T> {
    fn drop(&mut self) {
        self.shared.world.step_unless_panicking("Watcher::drop");
    }
}
