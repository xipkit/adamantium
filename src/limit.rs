mod keyed;
pub use keyed::KeyedLimit;

use std::{
    collections::VecDeque,
    fmt,
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
};

use crate::runtime::{
    Lock, World,
    wait::{Turn, WaitLine, Wake},
};

#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, race, tasks::shutdown};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Lets at most a number of calls run at the same time.
///
/// Clones share one limit, so hand a clone to each task that should count
/// against it. [`run`](Self::run) waits for a place, runs the work, and
/// frees the place when the work finishes or is dropped. Waiting calls are
/// served in the order they started waiting, and a call that arrives while
/// others wait queues behind them.
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
    state: Lock<State>,
    line: WaitLine,
}

struct State {
    taken: usize,
    // Oldest first. Only the call at the front may take places, so places
    // freed while it waits stay free until it has enough.
    waiting: VecDeque<Waiting>,
    next_ticket: u64,
}

struct Waiting {
    ticket: u64,
    places: usize,
    turn: Turn,
}

struct Take<'a> {
    limit: &'a Limit,
    places: usize,
    // Set while the call waits.
    ticket: Option<u64>,
}

struct Place {
    places: Arc<Places>,
    weight: usize,
}

impl Limit {
    /// A limit of `places` places. Zero is treated as one.
    pub fn new(places: usize) -> Self {
        Self {
            places: Arc::new(Places {
                limit: places.max(1),
                state: Lock::new(State {
                    taken: 0,
                    waiting: VecDeque::new(),
                    next_ticket: 0,
                }),
                line: WaitLine::named("limit.acquire"),
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
        Kind::Ordinary.run(self.take(1), work).await
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
            .run_until(self.take(1), cancellation, deadline, work)
            .await
    }

    /// The number of places that are free now.
    pub fn available(&self) -> usize {
        self.places
            .limit
            .saturating_sub(self.places.state.lock("Limit::available").taken)
    }

    fn take(&self, places: usize) -> Take<'_> {
        Take {
            limit: self,
            places,
            ticket: None,
        }
    }
}

impl Places {
    // A modeled limit used outside its execution leaves its state alone: the
    // wakers in its line can name tasks of a finished execution.
    fn is_foreign(&self) -> bool {
        let world = self.line.world();
        if world == World::current() {
            return false;
        }
        if !std::thread::panicking() {
            world.check();
        }
        true
    }
}

impl State {
    fn fits(&self, places: usize, limit: usize) -> bool {
        places <= limit.saturating_sub(self.taken)
    }

    // The front call's wake, once its places are free and unless it was
    // woken already.
    fn wake_front(&mut self, limit: usize) -> Option<Wake> {
        let free = limit.saturating_sub(self.taken);
        let front = self.waiting.front_mut()?;
        if front.places <= free {
            front.turn.take_wake()
        } else {
            None
        }
    }
}

impl Take<'_> {
    fn poll_take(&mut self, cx: &Context<'_>) -> Poll<Place> {
        let places = &self.limit.places;
        let mut state = places.state.lock("Limit::take");
        let Some(ticket) = self.ticket else {
            if state.waiting.is_empty() && state.fits(self.places, places.limit) {
                state.taken = state.taken.saturating_add(self.places);
                return Poll::Ready(self.place());
            }
            let ticket = state.next_ticket;
            state.next_ticket = ticket.wrapping_add(1);
            state.waiting.push_back(Waiting {
                ticket,
                places: self.places,
                turn: places.line.wait(cx),
            });
            self.ticket = Some(ticket);
            return Poll::Pending;
        };
        let at_front = state
            .waiting
            .front()
            .is_some_and(|front| front.ticket == ticket);
        if at_front && state.fits(self.places, places.limit) {
            let served = state.waiting.pop_front();
            state.taken = state.taken.saturating_add(self.places);
            let next = state.wake_front(places.limit);
            drop(state);
            self.ticket = None;
            // Held before the wake, so a panic there gives the places back.
            let place = self.place();
            if let Some(served) = served {
                served.turn.complete();
            }
            if let Some(next) = next {
                places.line.wake(next);
            }
            return Poll::Ready(place);
        }
        if let Some(waiting) = state
            .waiting
            .iter_mut()
            .find(|waiting| waiting.ticket == ticket)
        {
            waiting.turn.update(cx);
        }
        Poll::Pending
    }

    fn place(&self) -> Place {
        Place {
            places: Arc::clone(&self.limit.places),
            weight: self.places,
        }
    }
}

impl Future for Take<'_> {
    type Output = Place;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Place> {
        let take = self.get_mut();
        let world = take.limit.places.line.world();
        world.poll_cooperative(cx, |cx| take.poll_take(cx))
    }
}

impl Drop for Take<'_> {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let places = &self.limit.places;
        if places.is_foreign() {
            return;
        }
        let (left, next) = {
            let mut state = places.state.lock("Limit::leave");
            let index = state
                .waiting
                .iter()
                .position(|waiting| waiting.ticket == ticket);
            let left = index.and_then(|index| state.waiting.remove(index));
            // Only the front call can be held back by the places it needs.
            let next = if index == Some(0) {
                state.wake_front(places.limit)
            } else {
                None
            };
            (left, next)
        };
        drop(left);
        if let Some(next) = next {
            places.line.wake(next);
        }
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if self.places.is_foreign() {
            return;
        }
        let next = {
            let mut state = self.places.state.lock("Place::drop");
            state.taken = state.taken.saturating_sub(self.weight);
            state.wake_front(self.places.limit)
        };
        if let Some(next) = next {
            self.places.line.wake(next);
        }
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
