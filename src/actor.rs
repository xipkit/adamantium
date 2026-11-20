use std::fmt;

use crate::runtime::{Lock, Shared};

/// Shared state whose methods run one at a time.
///
/// [`Self::with`] gives a synchronous closure exclusive access to the state.
/// The closure cannot hold the lock across an await. Nested `with` or `read`
/// calls on any actor panic to prevent deadlock.
///
/// ```
/// # use adamantium::Actor;
/// #[derive(Default)]
/// struct Tally { hits: u64 }
/// impl Tally {
///     fn record(&mut self) { self.hits += 1; }
///     fn total(&self) -> u64 { self.hits }
/// }
///
/// let tally = Actor::new(Tally::default());
/// tally.with(Tally::record);
/// assert_eq!(tally.read(Tally::total), 1);
/// ```
///
/// Run async work outside the actor and record its result with `with`.
/// Use `AsyncActor` if the state must stay exclusively owned across awaits.
///
/// Clones share the same state. Remove any strong reference cycles before
/// dropping the last external handle to avoid leaking the state.
pub struct Actor<S> {
    state: Shared<Lock<S>>,
}

thread_local! {
    // Actor methods run synchronously, so nesting cannot cross threads.
    #[allow(clippy::disallowed_types, reason = "a per-thread counter, never shared")]
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "deterministic")]
shuttle::thread_local! {
    #[allow(clippy::disallowed_types, reason = "nesting belongs to a logical task, not its OS thread")]
    static MODEL_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[allow(
    clippy::disallowed_methods,
    reason = "an actor method is synchronous, so the task can't change threads while the count is held"
)]
fn depth() -> usize {
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        return MODEL_DEPTH.with(|value| value.get());
    }
    DEPTH.get()
}

#[allow(
    clippy::disallowed_methods,
    reason = "an actor method is synchronous, so the task can't change threads while the count is held"
)]
fn set_depth(depth: usize) {
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        MODEL_DEPTH.with(|value| value.set(depth));
        return;
    }
    DEPTH.set(depth);
}

const NESTED: &str = "adamantium: Actor::with was called inside another actor's method; \
an actor method can't call an actor, so call the second actor after the first returns";

struct Inside;

impl Inside {
    #[allow(
        clippy::panic,
        reason = "a nested actor call is a bug in the caller, and a panic names it where a deadlock would hang"
    )]
    fn enter() -> Self {
        let depth = depth();
        if depth > 0 {
            std::panic::panic_any(NESTED);
        }
        set_depth(depth + 1);
        Self
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        set_depth(depth().saturating_sub(1));
    }
}

impl<S> Actor<S> {
    /// An actor holding `state`.
    pub fn new(state: S) -> Self {
        Self {
            state: Shared::new(Lock::new(state)),
        }
    }

    /// Runs `f` with exclusive access to the state and returns its result.
    ///
    /// Other callers wait their turn. `f` can't await, because it is a
    /// plain closure, and it can't call `with` on any actor: doing so
    /// panics with a message that says which rule was broken. A panic
    /// inside `f` leaves the state as `f` left it; the next caller sees it.
    pub fn with<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let _inside = Inside::enter();
        let mut guard = self.state.lock();
        f(&mut guard)
    }

    /// Runs `f` with shared access to the state and returns its result.
    ///
    /// The same rules as [`with`](Self::with) apply. This form takes `&S`,
    /// so a method that only reads, such as `Tally::total`, can be passed
    /// by name.
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        let _inside = Inside::enter();
        let guard = self.state.lock();
        f(&guard)
    }

    /// The state, if this is the only handle to it; otherwise the actor back.
    pub fn into_inner(self) -> Result<S, Self> {
        self.state
            .try_unwrap()
            .map(Lock::into_inner)
            .map_err(|state| Self { state })
    }

    /// Whether another handle to the same state exists.
    pub fn is_shared(&self) -> bool {
        self.state.is_shared()
    }
}

impl<S: Clone> Actor<S> {
    /// A copy of the state as it is now.
    pub fn snapshot(&self) -> S {
        self.read(S::clone)
    }
}

impl<S> Clone for Actor<S> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<S: Default> Default for Actor<S> {
    fn default() -> Self {
        Self::new(S::default())
    }
}

impl<S: fmt::Debug> fmt::Debug for Actor<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.state.try_lock() {
            Some(state) => f.debug_tuple("Actor").field(&*state).finish(),
            None => f.write_str("Actor(<in use>)"),
        }
    }
}
