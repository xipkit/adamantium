//! Sending each event to every subscriber that keeps up.

use std::{collections::VecDeque, fmt, sync::Arc};

#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, shutdown};
use crate::{
    Closed,
    runtime::{Guard, Lock, World, wait::WaitQueue},
};

/// Sends each event to every subscriber, and disconnects a subscriber that
/// falls behind instead of waiting for it.
///
/// Each [`Feed`] has its own queue of at most `capacity` events.
/// [`send`](Self::send) never waits: it gives every feed that has room a
/// clone of the event, and disconnects every feed whose queue is full. A
/// disconnected feed receives the events already in its queue, then
/// [`Ended::Overflowed`], and never a later event. Whatever a feed receives
/// is therefore an unbroken run of the events sent since it subscribed, with
/// none missing in between.
///
/// Use a `Fanout` when a subscriber must neither hold up the publisher nor
/// silently miss an event. Use [`Topic`](crate::Topic) to make the publisher
/// wait for a slow subscriber instead, and [`Watch`](crate::Watch) when only
/// the latest value matters.
///
/// Clones share the subscribers, and every clone can send and subscribe.
/// Sends from all clones reach every feed in one order: the order in which
/// they took the fanout's lock, which for sends from one task is the order
/// they were made. Dropping the last `Fanout`, or calling
/// [`close`](Self::close), closes it: each feed receives the events already
/// in its queue and then [`Ended::Closed`]. Feeds don't hold the fanout
/// open, and dropping a feed unsubscribes it.
///
/// ```
/// use adamantium::{Ended, Fanout, Sent};
///
/// let prices = Fanout::new(2);
/// let mut display = prices.subscribe();
/// let mut stalled = prices.subscribe();
///
/// for price in [1, 2] {
///     prices.send(price).unwrap();
///     assert_eq!(display.try_receive(), Ok(Some(price)));
/// }
/// // `stalled` hasn't read anything, so its queue is full and the next send
/// // disconnects it. `display` still gets the event.
/// assert_eq!(prices.send(3), Ok(Sent { delivered: 1, disconnected: 1 }));
/// assert_eq!(display.try_receive(), Ok(Some(3)));
///
/// assert_eq!(stalled.try_receive(), Ok(Some(1)));
/// assert_eq!(stalled.try_receive(), Ok(Some(2)));
/// assert_eq!(stalled.try_receive(), Err(Ended::Overflowed));
/// ```
///
/// # Recovering from overflow
///
/// A feed that overflowed has missed events. To catch up, take a new feed
/// with [`Feed::resubscribe`] first, then read a snapshot of the state the
/// events describe, then apply the new feed's events. Subscribing before
/// the snapshot means no event falls between the two, but the new feed can
/// repeat a change the snapshot already includes, so applying an event
/// twice must do no harm.
///
/// ```
/// use adamantium::{Actor, Ended, Fanout, Map};
///
/// // The source of truth, and a fanout of its changes. Setting a key is
/// // safe to apply twice.
/// let stock = Actor::new(Map::<&str, u32>::new());
/// let changes = Fanout::new(1);
/// let set = |item, count| {
///     stock.with(|stock| stock.insert(item, count));
///     changes.send((item, count)).unwrap();
/// };
///
/// let mut feed = changes.subscribe();
/// let mut view = stock.snapshot();
/// set("pears", 3);
/// set("plums", 7); // The feed is full, so this disconnects it.
///
/// loop {
///     match feed.try_receive() {
///         Ok(Some((item, count))) => {
///             view.insert(item, count);
///         }
///         Ok(None) => break,
///         Err(Ended::Overflowed) => {
///             feed = feed.resubscribe();
///             view = stock.snapshot();
///         }
///         Err(Ended::Closed) => break,
///     }
/// }
/// assert_eq!(view, stock.snapshot());
/// ```
pub struct Fanout<T> {
    shared: Arc<Shared<T>>,
}

/// One subscriber to a [`Fanout`], made by [`Fanout::subscribe`] or
/// [`Feed::resubscribe`].
///
/// A feed receives, in order, the events sent after it subscribed, until
/// the fanout closes or the feed falls a full queue behind. Then it
/// receives the events still queued, followed by the reason it [`Ended`].
/// Dropping a feed unsubscribes it.
pub struct Feed<T> {
    shared: Arc<Shared<T>>,
    inbox: Arc<Inbox<T>>,
}

/// What [`Fanout::send`] did with an event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    /// The feeds that received the event.
    pub delivered: usize,
    /// The feeds whose queue was full, which this send disconnected.
    pub disconnected: usize,
}

/// Why a [`Feed`] receives no more events. A feed returns this once it has
/// received every event in its queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The fanout was closed.
    Closed,
    /// The feed's queue was full when an event was sent, so the feed was
    /// disconnected and missed that event and every later one. See
    /// [`Feed::resubscribe`].
    Overflowed,
}

/// Returned by [`Feed::receive_until`] when no event was received.
#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedError {
    /// The fanout was closed and the feed has received every queued event.
    Closed,
    /// The feed fell behind and was disconnected, and has received every
    /// queued event.
    Overflowed,
    /// The caller requested shutdown.
    Cancelled,
    /// The deadline elapsed.
    DeadlineExceeded,
}

struct Shared<T> {
    /// Held only inside this module's synchronous methods, which run no
    /// caller code but `T::clone`. A send holds it from start to finish, so
    /// every feed sees sends in the same order.
    state: Lock<State<T>>,
    capacity: usize,
    world: World,
}

struct State<T> {
    /// The connected feeds. A feed leaves when it overflows, when the
    /// fanout closes, and when it is dropped, so each one here is held.
    feeds: Vec<Arc<Inbox<T>>>,
    closed: bool,
    /// The live `Fanout` handles. The last one to drop closes the fanout.
    fanouts: usize,
}

/// A feed's queue. It is locked only after the fanout's lock or on its
/// own, and never while running caller code.
struct Inbox<T> {
    queue: Lock<Queue<T>>,
    /// Notified on every event and when the feed ends.
    arrived: WaitQueue,
}

struct Queue<T> {
    events: VecDeque<T>,
    /// Set once the feed is disconnected. Events queued before stay
    /// receivable.
    end: Option<Ended>,
}

impl<T> Shared<T> {
    /// Every operation locks, so this is where a handle carried into the
    /// deterministic harness is rejected, as in `Watch`. While unwinding the
    /// check is skipped, so a destructor can still close the fanout.
    fn lock(&self) -> Guard<'_, State<T>> {
        if !std::thread::panicking() {
            self.world.check();
        }
        self.state.lock()
    }

    fn subscribe(self: &Arc<Self>) -> Feed<T> {
        let mut state = self.lock();
        let end = state.closed.then_some(Ended::Closed);
        let inbox = Arc::new(Inbox::new(end));
        if !state.closed {
            state.feeds.push(Arc::clone(&inbox));
        }
        drop(state);
        Feed {
            shared: Arc::clone(self),
            inbox,
        }
    }

    fn close(&self) -> bool {
        self.close_with(self.lock())
    }

    fn close_with(&self, mut state: Guard<'_, State<T>>) -> bool {
        if std::mem::replace(&mut state.closed, true) {
            return false;
        }
        let feeds = std::mem::take(&mut state.feeds);
        for inbox in &feeds {
            inbox.end(Ended::Closed);
        }
        drop(state);
        for inbox in &feeds {
            inbox.arrived.notify_all();
        }
        true
    }
}

impl<T> State<T> {
    /// Disconnects the feeds whose queue is full and returns them, to be
    /// woken and dropped after the lock is released.
    #[must_use]
    fn cut_off_full(&mut self, capacity: usize) -> Vec<Arc<Inbox<T>>> {
        self.feeds
            .extract_if(.., |inbox| inbox.end_if_full(capacity))
            .collect()
    }
}

impl<T> Inbox<T> {
    fn new(end: Option<Ended>) -> Self {
        Self {
            queue: Lock::new(Queue {
                events: VecDeque::new(),
                end,
            }),
            arrived: WaitQueue::named("fanout.receive"),
        }
    }

    fn end(&self, end: Ended) {
        self.queue.lock().end = Some(end);
    }

    /// Ends the feed with [`Ended::Overflowed`] if its queue is full, and
    /// says whether it did. Only a send adds to a queue, under the fanout's
    /// lock, so a queue with room keeps it until that lock is released.
    fn end_if_full(&self, capacity: usize) -> bool {
        let mut queue = self.queue.lock();
        let full = queue.events.len() >= capacity;
        if full {
            queue.end = Some(Ended::Overflowed);
        }
        full
    }

    fn push(&self, event: T) {
        self.queue.lock().events.push_back(event);
    }
}

impl<T> Fanout<T> {
    /// A fanout whose feeds each queue at most `capacity` events. A
    /// capacity of zero is treated as one.
    pub fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Lock::new(State {
                    feeds: Vec::new(),
                    closed: false,
                    fanouts: 1,
                }),
                capacity: capacity.max(1),
                world: World::current(),
            }),
        }
    }

    /// Adds a feed. It receives the events sent from now on. If the fanout
    /// is closed, the feed has already ended with [`Ended::Closed`].
    pub fn subscribe(&self) -> Feed<T> {
        self.shared.subscribe()
    }

    /// The number of feeds that are still connected and held.
    pub fn subscriber_count(&self) -> usize {
        self.shared.lock().feeds.len()
    }

    /// Closes the fanout for every handle. Each feed receives the events in
    /// its queue and then [`Ended::Closed`], and later sends fail. Returns
    /// `false` if it was already closed.
    pub fn close(&self) -> bool {
        self.shared.close()
    }

    /// Whether the fanout is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }
}

impl<T: Clone> Fanout<T> {
    /// Sends `value` to every feed without waiting, and reports how many
    /// feeds received it and how many it disconnected because their queue
    /// was full. Hands the value back in [`Closed`] if the fanout is closed.
    ///
    /// With no feeds the value is dropped and nothing is delivered. The
    /// clones are made while the fanout's lock is held, before any feed
    /// receives the event, so a `T::clone` that panics delivers it to none.
    /// Because the lock is held, `T::clone` must not use this fanout:
    /// cloning a `Fanout` handle there, for one, waits for the lock forever.
    /// Destructors of `T` and the wakers of waiting feeds run after the lock
    /// is released, and can use it.
    pub fn send(&self, value: T) -> Result<Sent, Closed<T>> {
        // Declared before the guard, so that if a `T::clone` panics, the
        // clones already made drop after unwinding has released the lock:
        // their destructors are the caller's code.
        let mut events: Vec<T> = Vec::new();
        let mut state = self.shared.lock();
        if state.closed {
            drop(state);
            return Err(Closed(value));
        }
        let cut_off = state.cut_off_full(self.shared.capacity);
        // One clone for each feed but the last, which takes `value`. With no
        // feeds `value` stays here, to drop once the lock is released.
        events.extend(state.feeds.iter().skip(1).map(|_| value.clone()));
        events.push(value);
        for inbox in &state.feeds {
            if let Some(event) = events.pop() {
                inbox.push(event);
            }
        }
        // Wakers are the caller's code too, so they run after the lock is
        // released.
        let delivered = state.feeds.clone();
        drop(state);
        for inbox in delivered.iter().chain(&cut_off) {
            inbox.arrived.notify_all();
        }
        let sent = Sent {
            delivered: delivered.len(),
            disconnected: cut_off.len(),
        };
        // A feed dropped since the lock was released leaves its queue to the
        // last of these handles, which drops it here, outside the lock.
        drop(delivered);
        drop(cut_off);
        drop(events);
        Ok(sent)
    }
}

impl<T> Feed<T> {
    /// A new feed on the same fanout, which receives the events sent from
    /// now on, while this one keeps its own. If the fanout is closed, the
    /// new feed has already ended with [`Ended::Closed`].
    ///
    /// Use it to recover from [`Ended::Overflowed`]: resubscribe first, then
    /// read a snapshot of the state, as the [`Fanout`] example shows.
    pub fn resubscribe(&self) -> Feed<T> {
        self.shared.subscribe()
    }

    /// The number of events waiting.
    pub fn len(&self) -> usize {
        self.queue().events.len()
    }

    /// Whether no events are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the feed gets no more events, because the fanout closed or
    /// the feed overflowed. Events already queued can still be received.
    pub fn is_ended(&self) -> bool {
        self.queue().end.is_some()
    }

    /// Receives the next event, waiting while none is queued. Once the feed
    /// has ended and its queue is empty, returns why it [`Ended`].
    ///
    /// Dropping the returned future loses nothing: an event stays queued
    /// until it is returned.
    pub async fn receive(&mut self) -> Result<T, Ended> {
        crate::runtime::in_world(self.shared.world, async move {
            loop {
                if let Some(received) = self.next() {
                    return received;
                }
                // Listen before checking again, so an event sent between
                // the two checks still wakes this wait.
                let listener = self.inbox.arrived.listen();
                if let Some(received) = self.next() {
                    return received;
                }
                listener.await;
            }
        })
        .await
    }

    /// Receives without waiting. Returns `Ok(None)` while the queue is
    /// empty and the feed is connected, and why it [`Ended`] once it has
    /// ended and its queue is empty.
    pub fn try_receive(&mut self) -> Result<Option<T>, Ended> {
        self.next().transpose()
    }

    /// Receives the next event, as in [`receive`](Self::receive), until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline needs Tokio's time driver.
    ///
    /// A request already present when the call is made, or at any later
    /// poll, wins over a queued event, and cancellation wins over an
    /// equally ready deadline, as in [`Tasks::run_until`](crate::Tasks::run_until).
    /// Ending early leaves the queue as it was.
    #[cfg(feature = "tokio")]
    pub async fn receive_until(
        &mut self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<T, FeedError> {
        crate::runtime::in_world(self.shared.world, async move {
            #[expect(
                clippy::disallowed_methods,
                reason = "receive_until checks shutdown before taking a queued event"
            )]
            let reason = tokio::select! {
                biased;
                reason = shutdown(cancellation, deadline) => reason,
                received = self.receive() => return received.map_err(FeedError::from),
            };
            Err(match reason {
                Shutdown::Cancelled => FeedError::Cancelled,
                Shutdown::DeadlineExceeded => FeedError::DeadlineExceeded,
            })
        })
        .await
    }

    /// `Some(Ok(event))` for a queued event, `Some(Err(end))` once ended
    /// with nothing queued, and `None` to wait.
    fn next(&mut self) -> Option<Result<T, Ended>> {
        let mut queue = self.queue();
        match queue.events.pop_front() {
            Some(event) => Some(Ok(event)),
            None => queue.end.map(Err),
        }
    }

    fn queue(&self) -> Guard<'_, Queue<T>> {
        if !std::thread::panicking() {
            self.shared.world.check();
        }
        self.inbox.queue.lock()
    }
}

impl<T> Clone for Fanout<T> {
    fn clone(&self) -> Self {
        let mut state = self.shared.lock();
        // A count of every handle in memory can't reach usize::MAX.
        state.fanouts = state.fanouts.saturating_add(1);
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Fanout<T> {
    fn drop(&mut self) {
        if std::thread::panicking()
            && self.shared.world.is_model()
            && self.shared.world != World::current()
        {
            return;
        }
        let mut state = self.shared.lock();
        state.fanouts = state.fanouts.saturating_sub(1);
        if state.fanouts == 0 {
            self.shared.close_with(state);
        }
    }
}

impl<T> Drop for Feed<T> {
    fn drop(&mut self) {
        if std::thread::panicking()
            && self.shared.world.is_model()
            && self.shared.world != World::current()
        {
            return;
        }
        let mut state = self.shared.lock();
        // The list's handle is never the last, since this feed holds one, so
        // removing it drops no event under the lock. The queued events drop
        // with `self.inbox`, after the lock is released.
        state.feeds.retain(|inbox| !Arc::ptr_eq(inbox, &self.inbox));
    }
}

impl<T> fmt::Debug for Fanout<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `try_lock`, as in `Watch`'s `Debug`, so formatting from inside
        // `T::clone` can't deadlock.
        self.shared.world.check();
        let Some(state) = self.shared.state.try_lock() else {
            return f.write_str("Fanout(<in use>)");
        };
        f.debug_struct("Fanout")
            .field("subscribers", &state.feeds.len())
            .field("capacity", &self.shared.capacity)
            .field("closed", &state.closed)
            .finish()
    }
}

impl<T> fmt::Debug for Feed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let queue = self.queue();
        f.debug_struct("Feed")
            .field("len", &queue.events.len())
            .field("ended", &queue.end)
            .finish()
    }
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the fanout is closed and the feed is drained"),
            Self::Overflowed => {
                f.write_str("the feed fell behind and was disconnected, and is drained")
            }
        }
    }
}

impl std::error::Error for Ended {}

#[cfg(feature = "tokio")]
impl From<Ended> for FeedError {
    fn from(end: Ended) -> Self {
        match end {
            Ended::Closed => Self::Closed,
            Ended::Overflowed => Self::Overflowed,
        }
    }
}

#[cfg(feature = "tokio")]
impl fmt::Display for FeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => Ended::Closed.fmt(f),
            Self::Overflowed => Ended::Overflowed.fmt(f),
            Self::Cancelled => f.write_str("receive cancelled"),
            Self::DeadlineExceeded => f.write_str("receive deadline exceeded"),
        }
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for FeedError {}
