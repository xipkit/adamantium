use std::{
    fmt,
    sync::{Arc, PoisonError},
};

/// Shared state whose methods run one at a time.
///
/// An `Actor` wraps a value that several tasks use. [`with`](Self::with)
/// runs a closure with exclusive access to the value. The closure is
/// synchronous, so the lock can't be held across an await, and a method
/// can't call another actor: a nested `with` or `read` on any actor, this
/// one or another, panics instead of risking a deadlock. Those two rules are what
/// make a program with actors free of lock deadlocks and of locks held
/// through a cancellation.
///
/// Write the state as a plain struct with plain `&mut self` methods, which
/// are testable with no concurrency at all, and call them through `with`:
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
/// Work that waits, such as a network call, happens in a task; the actor
/// records its result afterwards.
pub struct Actor<S> {
    #[allow(
        clippy::disallowed_types,
        reason = "the one lock in the fragment, held only inside `with`"
    )]
    state: Arc<std::sync::Mutex<S>>,
}

thread_local! {
    /// How many actor methods the current thread is inside. A method is
    /// synchronous, so a task can't move threads while it runs one.
    #[allow(clippy::disallowed_types, reason = "a per-thread counter, never shared")]
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

const NESTED: &str = "adamantium: Actor::with was called inside another actor's method; \
an actor method can't call an actor, so call the second actor after the first returns";

/// Marks the current thread as inside an actor method until dropped.
struct Inside;

impl Inside {
    #[allow(
        clippy::panic,
        reason = "a nested actor call is a bug in the caller, and a panic names it where a deadlock would hang"
    )]
    fn enter() -> Self {
        let depth = DEPTH.get();
        if depth > 0 {
            std::panic::panic_any(NESTED);
        }
        DEPTH.set(depth + 1);
        Self
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get().saturating_sub(1));
    }
}

impl<S> Actor<S> {
    /// An actor holding `state`.
    #[allow(clippy::disallowed_types, reason = "see the state field")]
    pub fn new(state: S) -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(state)),
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
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    /// Runs `f` with shared access to the state and returns its result.
    ///
    /// The same rules as [`with`](Self::with) apply. This form takes `&S`,
    /// so a method that only reads, such as `Tally::total`, can be passed
    /// by name.
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        let _inside = Inside::enter();
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&guard)
    }

    /// The state, if this is the only handle to it; otherwise the actor back.
    pub fn into_inner(self) -> Result<S, Self> {
        Arc::try_unwrap(self.state)
            .map(|lock| lock.into_inner().unwrap_or_else(PoisonError::into_inner))
            .map_err(|state| Self { state })
    }

    /// Whether another handle to the same state exists.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.state) > 1
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
            state: Arc::clone(&self.state),
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
            Ok(state) => f.debug_tuple("Actor").field(&*state).finish(),
            Err(_) => f.write_str("Actor(<in use>)"),
        }
    }
}
