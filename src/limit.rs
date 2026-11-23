mod keyed;
pub use keyed::KeyedLimit;

use std::{
    collections::BTreeMap,
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
};

use crate::runtime::{Lock, World, wait::WaitQueue};

#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, race, tasks::shutdown};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Lets calls run at the same time while together they take at most a
/// number of places.
///
/// Clones share one limit, so hand a clone to each task that should count
/// against it. [`run`](Self::run) waits for a place, runs the work, and
/// frees the place when the work finishes or is dropped. A call made through
/// [`weighted`](Self::weighted) takes several places at once, for a budget
/// whose calls cost different amounts, such as bytes in flight.
///
/// Waiting calls are served in the order they started waiting, whatever
/// their weights, so small calls can't starve a large one. The price is that
/// a call queues behind the calls already waiting, even when enough places
/// are free for it: while a large call waits for its places, the calls that
/// come after it wait too.
///
/// Work may enter a [`KeyedLimit`]. Starting another [`Limit`] inside the work
/// panics. A task that holds a place while it waits for a second one can
/// deadlock with a task that takes them in the other order, and a limit of
/// one deadlocks with itself. The check sees only a call made by the work
/// itself. A task that the work spawns and waits for is not checked, so
/// don't let such a task use a limit that the work already holds. It would
/// queue behind any call already waiting, even while places are free, and
/// that call can wait for the places the work holds.
///
/// To limit the items of one collection, use `concurrently`, which needs no
/// shared limit.
///
/// ```
/// use adamantium::Limit;
/// # async fn query(n: u32) -> u32 { n }
/// # async fn upload(bytes: &[u8]) -> usize { bytes.len() }
///
/// # async fn example() -> Result<(), adamantium::Overweight> {
/// let connections = Limit::new(4);
/// let rows = connections.run(|| query(7)).await;
/// assert_eq!(rows, 7);
///
/// let in_flight = Limit::new(64 * 1024);
/// let body = vec![0; 4096];
/// let sent = in_flight.weighted(body.len())?.run(|| upload(&body)).await;
/// assert_eq!(sent, 4096);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Limit {
    places: Arc<Places>,
}

/// Calls under a [`Limit`] that each take the same number of places.
///
/// The handle holds nothing: it takes no place and no position in the
/// queue. A call's position is set when its `run` or `run_until` is first
/// polled. [`Limit::weighted`] checks the weight once, so a call made here
/// never fails for asking too much. The handle is `Copy`, to make as many
/// calls as you like.
#[must_use = "a Weighted handle takes no places until one of its calls runs"]
#[derive(Clone, Copy, Debug)]
pub struct Weighted<'a> {
    limit: &'a Limit,
    places: usize,
}

/// Returned by [`Limit::weighted`] for a weight above the limit's total,
/// which no call could ever take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overweight {
    /// The places a call asked for.
    pub places: usize,
    /// The places of the limit.
    pub limit: usize,
}

struct Places {
    limit: usize,
    state: Lock<State>,
    world: World,
}

struct State {
    taken: usize,
    // By ticket, so oldest first, and a call that gives up leaves without a
    // scan. Only the call at the front may take places, so places freed
    // while it waits stay free until it has enough.
    waiting: BTreeMap<u64, Waiting>,
    // Counts up from zero. A u64 doesn't wrap in any real run.
    next_ticket: u64,
}

struct Waiting {
    places: usize,
    // Only this call listens, so notify_one wakes exactly it.
    woken: Arc<WaitQueue>,
}

enum Entered<'a> {
    Taken(Place),
    Waiting(Queued<'a>),
}

// A call's place in the queue. Dropping it before the call is served
// leaves the queue.
struct Queued<'a> {
    limit: &'a Limit,
    ticket: u64,
    places: usize,
    woken: Arc<WaitQueue>,
    served: bool,
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
                    waiting: BTreeMap::new(),
                    next_ticket: 0,
                }),
                world: World::current(),
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

    /// Calls that each take `places` places at once. Zero is treated as one.
    ///
    /// Returns [`Overweight`] without waiting when `places` is above the
    /// limit's total, since no such call could ever run. The handle takes no
    /// place and no position in the queue until one of its calls is polled.
    ///
    /// The weight is checked here, before the nesting check that runs when a
    /// call is first polled. Inside work that holds a place, a weight above
    /// the total returns [`Overweight`], and a call that fits panics when
    /// polled, as [`run`](Self::run) does.
    pub fn weighted(&self, places: usize) -> Result<Weighted<'_>, Overweight> {
        let places = places.max(1);
        if places > self.places.limit {
            return Err(Overweight {
                places,
                limit: self.places.limit,
            });
        }
        Ok(Weighted {
            limit: self,
            places,
        })
    }

    /// The number of places that are free now.
    ///
    /// A new call can't take them while another call waits, even when they
    /// are enough for it.
    pub fn available(&self) -> usize {
        self.places
            .limit
            .saturating_sub(self.places.state.lock("Limit::available").taken)
    }

    async fn take(&self, places: usize) -> Place {
        self.places
            .world
            .cooperative(async {
                match self.enter(places) {
                    Entered::Taken(place) => place,
                    Entered::Waiting(queued) => queued.wait().await,
                }
            })
            .await
    }

    // Takes the places at once only when nobody waits and they fit.
    // Otherwise the call joins the back of the queue.
    fn enter(&self, places: usize) -> Entered<'_> {
        let mut state = self.places.state.lock("Limit::take");
        if state.waiting.is_empty() && state.fits(places, self.places.limit) {
            state.taken = state.taken.saturating_add(places);
            return Entered::Taken(self.place(places));
        }
        let ticket = state.next_ticket;
        state.next_ticket = ticket.wrapping_add(1);
        let woken = Arc::new(WaitQueue::named("limit.acquire"));
        state.waiting.insert(
            ticket,
            Waiting {
                places,
                woken: Arc::clone(&woken),
            },
        );
        Entered::Waiting(Queued {
            limit: self,
            ticket,
            places,
            woken,
            served: false,
        })
    }

    fn place(&self, weight: usize) -> Place {
        Place {
            places: Arc::clone(&self.places),
            weight,
        }
    }
}

impl Weighted<'_> {
    /// The places each call takes.
    pub fn places(self) -> usize {
        self.places
    }

    /// Waits until the places are free, runs `work`, and returns its output.
    ///
    /// `work` is called only once the places are taken. Dropping the future
    /// frees them, or gives up the wait for them.
    ///
    /// # Panics
    ///
    /// Panics when polled inside work holding a [`Limit`] or a [`KeyedLimit`].
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the work and its future are"
    )]
    pub async fn run<F, Fut>(self, work: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        Kind::Ordinary.run(self.limit.take(self.places), work).await
    }

    /// Runs like [`run`](Self::run), but waits and works only until
    /// `cancellation` is requested or `deadline` passes, with the results of
    /// [`Limit::run_until`].
    ///
    /// # Panics
    ///
    /// Panics when polled inside work holding a [`Limit`] or a [`KeyedLimit`].
    #[cfg(feature = "tokio")]
    pub async fn run_until<F, Fut, T, E>(
        self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        work: F,
    ) -> Result<T, ScopeError<E>>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        Kind::Ordinary
            .run_until(self.limit.take(self.places), cancellation, deadline, work)
            .await
    }
}

impl State {
    fn fits(&self, places: usize, limit: usize) -> bool {
        places <= limit.saturating_sub(self.taken)
    }

    // The front call's queue, once its places are free. Notify it after
    // releasing the lock, since notify_one is a scheduling step.
    fn front_to_wake(&self, limit: usize) -> Option<Arc<WaitQueue>> {
        let (_, front) = self.waiting.first_key_value()?;
        self.fits(front.places, limit)
            .then(|| Arc::clone(&front.woken))
    }

    fn is_front(&self, ticket: u64) -> bool {
        self.waiting
            .first_key_value()
            .is_some_and(|(&front, _)| front == ticket)
    }
}

impl Queued<'_> {
    // Checks again after listening, as WaitQueue::wait_for does, so a wake
    // between the check and the listen isn't lost.
    async fn wait(mut self) -> Place {
        loop {
            if let Some(place) = self.try_serve() {
                return place;
            }
            let waiter = self.woken.listen();
            if let Some(place) = self.try_serve() {
                return place;
            }
            waiter.await;
        }
    }

    fn try_serve(&mut self) -> Option<Place> {
        let shared = &self.limit.places;
        let next = {
            let mut state = shared.state.lock("Limit::take");
            if !(state.is_front(self.ticket) && state.fits(self.places, shared.limit)) {
                return None;
            }
            drop(state.waiting.pop_first());
            state.taken = state.taken.saturating_add(self.places);
            state.front_to_wake(shared.limit)
        };
        self.served = true;
        // Held before the wake, so a panic there gives the places back.
        let place = self.limit.place(self.places);
        if let Some(next) = next {
            next.notify_one();
        }
        Some(place)
    }
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        if self.served {
            return;
        }
        let shared = &self.limit.places;
        shared.world.check_unless_panicking();
        if shared.world.unwinding_in_foreign_world() {
            return;
        }
        let next = {
            let mut state = shared.state.lock("Limit::leave");
            let was_front = state.is_front(self.ticket);
            drop(state.waiting.remove(&self.ticket));
            // Only the front call can be held back by the places it needs.
            if was_front {
                state.front_to_wake(shared.limit)
            } else {
                None
            }
        };
        if let Some(next) = next {
            next.notify_one();
        }
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.places.world.check_unless_panicking();
        if self.places.world.unwinding_in_foreign_world() {
            return;
        }
        let next = {
            let mut state = self.places.state.lock("Place::drop");
            state.taken = state.taken.saturating_sub(self.weight);
            state.front_to_wake(self.places.limit)
        };
        if let Some(next) = next {
            next.notify_one();
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

impl fmt::Display for Overweight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a call of {} places can never run under a limit of {}",
            self.places, self.limit
        )
    }
}

impl std::error::Error for Overweight {}

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
