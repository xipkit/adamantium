//! A limit on how many calls run at the same time.

mod keyed;
pub use keyed::KeyedLimit;

use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
};

use crate::runtime::{Lock, wait::WaitQueue};

#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, race, tasks::shutdown};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Lets at most a number of calls run at the same time.
///
/// Clones share one limit, so hand a clone to each task that should count
/// against it. [`run`](Self::run) waits for a place, runs the work, and
/// frees the place when the work finishes or is dropped. Waiting calls are
/// not served in the order they arrived.
///
/// Work may enter a [`KeyedLimit`]. Starting another [`Limit`] inside the work
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
                freed: WaitQueue::named("limit.acquire"),
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
    /// Panics when polled inside work holding a [`Limit`] or a [`KeyedLimit`].
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the work and its future are"
    )]
    pub async fn run<F, Fut>(&self, work: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        Kind::Ordinary.run(self.take(), work).await
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
    /// Panics when polled inside work holding a [`Limit`] or a [`KeyedLimit`].
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
        Kind::Ordinary
            .run_until(self.take(), cancellation, deadline, work)
            .await
    }

    /// The number of places that are free now.
    pub fn available(&self) -> usize {
        self.places.limit.saturating_sub(*self.places.taken.lock())
    }

    async fn take(&self) -> Place {
        self.places.freed.wait_for(|| self.try_take()).await
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

#[derive(Clone, Copy)]
enum Kind {
    Ordinary,
    Keyed,
}

thread_local! {
    #[allow(clippy::disallowed_types, reason = "per-poll state, never shared")]
    static HOLDING: std::cell::Cell<Option<Kind>> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "deterministic")]
shuttle::thread_local! {
    #[allow(clippy::disallowed_types, reason = "per-poll state belongs to the logical task")]
    static MODEL_HOLDING: std::cell::Cell<Option<Kind>> = const { std::cell::Cell::new(None) };
}

#[allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "access is scoped to one poll"
)]
fn with_holding<T>(f: impl FnOnce(&std::cell::Cell<Option<Kind>>) -> T) -> T {
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        return MODEL_HOLDING.with(f);
    }
    HOLDING.with(f)
}

struct Restore(Option<Kind>);

impl Drop for Restore {
    fn drop(&mut self) {
        with_holding(|holding| holding.set(self.0));
    }
}

#[allow(
    clippy::future_not_send,
    reason = "Send follows the supplied futures and work"
)]
impl Kind {
    async fn run<P, Fut: Future>(
        self,
        take: impl Future<Output = P>,
        work: impl FnOnce() -> Fut,
    ) -> Fut::Output {
        self.checked(async {
            let _place = take.await;
            self.poll_work(work).await
        })
        .await
    }

    #[cfg(feature = "tokio")]
    async fn run_until<P, Fut: Future<Output = Result<T, E>>, T, E>(
        self,
        take: impl Future<Output = P>,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        work: impl FnOnce() -> Fut,
    ) -> Result<T, ScopeError<E>> {
        self.checked(async {
            let mut stop = pin!(shutdown(cancellation, deadline));
            let _place = race((async { Err(stop.as_mut().await) }, async { Ok(take.await) }))
                .await
                .map_err(|reason| reason.scope_error())?;
            // Check shutdown again after acquisition, before invoking the closure.
            race((async { Err(stop.await.scope_error()) }, async {
                self.poll_work(work).await.map_err(ScopeError::Failed)
            }))
            .await
        })
        .await
    }

    async fn checked<F: Future>(self, future: F) -> F::Output {
        let mut future = pin!(future);
        poll_fn(|cx| {
            let outer = with_holding(|holding| holding.get());
            assert!(
                matches!((outer, self), (None, _) | (Some(Kind::Ordinary), Kind::Keyed)),
                "adamantium: work already holds a place under a limit; only Limit enclosing KeyedLimit is allowed"
            );
            future.as_mut().poll(cx)
        }).await
    }

    async fn poll_work<F: Future>(self, work: impl FnOnce() -> F) -> F::Output {
        // The closure itself can poll a nested call before returning its future.
        let mut future = pin!(async { work().await });
        poll_fn(|cx| {
            let _restore = Restore(with_holding(|holding| holding.replace(Some(self))));
            future.as_mut().poll(cx)
        })
        .await
    }
}
