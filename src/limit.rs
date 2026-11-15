//! A limit on how many calls run at the same time.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use crate::runtime::{Lock, wait::WaitQueue};

#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, race, tasks::shutdown};
#[cfg(feature = "tokio")]
use std::pin::pin;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Lets at most a number of calls run at the same time.
///
/// Clones share one limit, so hand a clone to each task that should count
/// against it. [`run`](Self::run) waits for a place, runs the work, and
/// frees the place when the work finishes or is dropped. Waiting calls are
/// not served in the order they arrived.
///
/// The work of one call can't start another call on any limit: doing so
/// panics. A task that holds a place while it waits for a second one can
/// deadlock with a task that takes them in the other order, and a limit of
/// one deadlocks with itself. The check sees only a call made by the work
/// itself. A task that the work spawns and waits for is not checked, so
/// don't let such a task use a limit that the work already holds.
///
/// To limit the items of one collection, use `concurrently`, which needs no
/// shared limit.
///
/// ```
/// use adamantium::Limit;
/// # async fn query(n: u32) -> u32 { n }
///
/// # async fn example() {
/// let connections = Limit::new(4);
/// let rows = connections.run(|| query(7)).await;
/// assert_eq!(rows, 7);
/// # }
/// ```
#[derive(Clone)]
pub struct Limit {
    places: Arc<Places>,
}

struct Places {
    limit: usize,
    taken: Lock<usize>,
    freed: WaitQueue,
}

/// A place under a limit, freed when dropped.
struct Place {
    places: Arc<Places>,
}

impl Limit {
    /// A limit of `places` calls at a time. Zero is treated as one.
    pub fn new(places: usize) -> Self {
        Self {
            places: Arc::new(Places {
                limit: places.max(1),
                taken: Lock::new(0),
                freed: WaitQueue::new(),
            }),
        }
    }

    /// Waits for a place, runs `work`, and returns its output.
    ///
    /// `work` is called only once a place is free. Dropping the future
    /// frees the place, or gives up the wait for one.
    ///
    /// # Panics
    ///
    /// Panics if it is called by the work of another `run` or `run_until`.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the work and its future are"
    )]
    pub async fn run<F, Fut>(&self, work: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        refuse_nesting();
        let _place = self.take().await;
        Held { work: work() }.await
    }

    /// Runs like [`run`](Self::run), but waits and works only until
    /// `cancellation` is requested or `deadline` passes. Pass `None` for no
    /// deadline.
    ///
    /// Returns `ScopeError::Failed` with the error of `work`, or `Cancelled`
    /// or `DeadlineExceeded`. A request already present wins, so a cancelled
    /// call never takes a place or starts its work, and cancellation wins
    /// over an equally ready deadline. Work that is still running when the
    /// call stops is dropped.
    ///
    /// # Panics
    ///
    /// Panics if it is called by the work of another `run` or `run_until`.
    #[cfg(feature = "tokio")]
    pub async fn run_until<F, Fut, T, E>(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        work: F,
    ) -> Result<T, ScopeError<E>>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        refuse_nesting();
        let mut stop = pin!(shutdown(cancellation, deadline));
        let place = race((async { Err(stop.as_mut().await) }, async {
            Ok(self.take().await)
        }))
        .await;
        let _place = match place {
            Ok(place) => place,
            Err(reason) => return Err(reason.scope_error()),
        };
        // Calling `work` inside the second future means a shutdown that wins
        // this poll never calls it.
        let result = race((async { Err(stop.as_mut().await) }, async {
            Ok(Held { work: work() }.await)
        }))
        .await;
        match result {
            Ok(result) => result.map_err(ScopeError::Failed),
            Err(reason) => Err(reason.scope_error()),
        }
    }

    /// The number of places that are free now.
    pub fn available(&self) -> usize {
        self.places.limit.saturating_sub(*self.places.taken.lock())
    }

    async fn take(&self) -> Place {
        loop {
            if let Some(place) = self.try_take() {
                return place;
            }
            let freed = self.places.freed.listen();
            // Registration precedes the recheck, so a freed place cannot be
            // missed.
            if let Some(place) = self.try_take() {
                return place;
            }
            freed.await;
        }
    }

    fn try_take(&self) -> Option<Place> {
        let mut taken = self.places.taken.lock();
        if *taken >= self.places.limit {
            return None;
        }
        *taken = taken.saturating_add(1);
        Some(Place {
            places: Arc::clone(&self.places),
        })
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        {
            let mut taken = self.places.taken.lock();
            *taken = taken.saturating_sub(1);
        }
        self.places.freed.notify_one();
    }
}

impl fmt::Debug for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Limit")
            .field("places", &self.places.limit)
            .field("available", &self.available())
            .finish()
    }
}

thread_local! {
    /// Whether the current thread is polling work that holds a place. The
    /// flag is set only for the length of one poll, so it stays correct
    /// when a task moves to another thread between polls.
    #[allow(clippy::disallowed_types, reason = "a per-thread flag, never shared")]
    static HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "deterministic")]
shuttle::thread_local! {
    #[allow(clippy::disallowed_types, reason = "the flag belongs to a logical task, not its OS thread")]
    static MODEL_HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[allow(
    clippy::disallowed_methods,
    reason = "the flag is read during the poll that set it"
)]
fn holding() -> bool {
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        return MODEL_HOLDING.with(|value| value.get());
    }
    HOLDING.get()
}

#[allow(
    clippy::disallowed_methods,
    reason = "the flag is set and cleared within one poll"
)]
fn set_holding(holding: bool) {
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        MODEL_HOLDING.with(|value| value.set(holding));
        return;
    }
    HOLDING.set(holding);
}

const NESTED: &str = "adamantium: Limit::run was called by work that already holds a place under a limit; \
a call can hold one place at a time, so finish the first call before starting the second";

#[allow(
    clippy::panic,
    reason = "a nested call is a bug in the caller, and a panic names it where a deadlock would hang"
)]
fn refuse_nesting() {
    if holding() {
        std::panic::panic_any(NESTED);
    }
}

pin_project_lite::pin_project! {
    /// Marks the thread as holding a place while it polls `work`.
    struct Held<Fut> {
        #[pin]
        work: Fut,
    }
}

/// Clears the mark when the poll ends, including by a panic.
struct Polling;

impl Drop for Polling {
    fn drop(&mut self) {
        set_holding(false);
    }
}

impl<Fut: Future> Future for Held<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        set_holding(true);
        let _polling = Polling;
        self.project().work.poll(context)
    }
}
