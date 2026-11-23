use std::{collections::VecDeque, fmt, sync::Arc};

#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, before_shutdown};
use crate::{
    Closed,
    runtime::{Guard, Lock, World, wait::WaitQueue},
};

/// Publishes events to subscribers without waiting for them.
///
/// [`Self::send`] queues a copy for each feed with room and disconnects full
/// feeds. All feeds see the same send order. Each feed drains its queue
/// before reporting [`Ended::Overflowed`] or [`Ended::Closed`].
///
/// Clones share the subscribers. [`Self::close`] or dropping the last
/// publisher closes the fanout. Dropping a feed unsubscribes it.
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
/// After overflow, [`Feed::resubscribe`] before taking a snapshot so no
/// changes are missed. Events may repeat changes included in the snapshot,
/// so they must be safe to apply twice.
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

/// Counts from [`Fanout::send`]. `delivered` counts feeds that got the event.
/// `disconnected` counts feeds whose queue was full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    pub delivered: usize,
    pub disconnected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Closed,
    Overflowed,
}

#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedError {
    Closed,
    Overflowed,
    Cancelled,
    DeadlineExceeded,
}

struct Shared<T> {
    // Each send holds this lock so all feeds see the same order.
    // T::clone is the only caller code run under it.
    state: Lock<State<T>>,
    capacity: usize,
    world: World,
}

struct State<T> {
    feeds: Vec<Arc<Inbox<T>>>,
    closed: bool,
    fanouts: usize,
}

// Lock the inbox alone or after the fanout lock. Run no caller code here.
struct Inbox<T> {
    queue: Lock<Queue<T>>,
    arrived: WaitQueue,
}

struct Queue<T> {
    events: VecDeque<T>,
    end: Option<Ended>,
}

impl<T> Shared<T> {
    // Every operation locks, so this is where a handle carried into the
    // deterministic harness is rejected, as in `Watch`. While unwinding the
    // check is skipped, so a destructor can still close the fanout.
    fn lock(&self, operation: &'static str) -> Guard<'_, State<T>> {
        self.world.check_unless_panicking();
        self.state.lock(operation)
    }

    fn subscribe(self: &Arc<Self>) -> Feed<T> {
        let mut state = self.lock("Fanout::subscribe");
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
        self.close_with(self.lock("Fanout::close"))
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
    // Disconnects the feeds whose queue is full and returns them, to be
    // woken and dropped after the lock is released.
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
        self.queue.lock("Inbox::end").end = Some(end);
    }

    // Ends the feed with [`Ended::Overflowed`] if its queue is full, and
    // says whether it did. Only a send adds to a queue, under the fanout's
    // lock, so a queue with room keeps it until that lock is released.
    fn end_if_full(&self, capacity: usize) -> bool {
        let mut queue = self.queue.lock("Inbox::end_if_full");
        let full = queue.events.len() >= capacity;
        if full {
            queue.end = Some(Ended::Overflowed);
        }
        full
    }

    fn push(&self, event: T) {
        self.queue.lock("Inbox::push").events.push_back(event);
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
        self.shared.lock("Fanout::subscriber_count").feeds.len()
    }

    /// Closes the fanout for every handle. Each feed receives the events in
    /// its queue and then [`Ended::Closed`], and later sends fail. Returns
    /// `false` if it was already closed.
    pub fn close(&self) -> bool {
        self.shared.close()
    }

    /// Whether the fanout is closed.
    pub fn is_closed(&self) -> bool {
        self.shared.lock("Fanout::is_closed").closed
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
        // Declare clones before the guard so their destructors run after unlock
        // if T::clone panics.
        let mut events: Vec<T> = Vec::new();
        let mut state = self.shared.lock("Fanout::send");
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
        // Wakers can run caller code, so wake after releasing the lock.
        let delivered = state.feeds.clone();
        drop(state);
        for inbox in delivered.iter().chain(&cut_off) {
            inbox.arrived.notify_all();
        }
        let sent = Sent {
            delivered: delivered.len(),
            disconnected: cut_off.len(),
        };
        // Drop these handles outside the lock. One may own the last queue reference.
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
        self.queue("Feed::len").events.len()
    }

    /// Whether no events are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the feed gets no more events, because the fanout closed or
    /// the feed overflowed. Events already queued can still be received.
    pub fn is_ended(&self) -> bool {
        self.queue("Feed::is_ended").end.is_some()
    }

    /// Receives the next event, waiting while none is queued. Once the feed
    /// has ended and its queue is empty, returns why it [`Ended`].
    ///
    /// Dropping the returned future loses nothing: an event stays queued
    /// until it is returned.
    pub async fn receive(&mut self) -> Result<T, Ended> {
        self.inbox.arrived.wait_for(|| self.next()).await
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
            before_shutdown(cancellation, deadline, self.receive())
                .await
                .map_err(Shutdown::feed_error)?
                .map_err(FeedError::from)
        })
        .await
    }

    fn next(&self) -> Option<Result<T, Ended>> {
        let mut queue = self.queue("Feed::next");
        match queue.events.pop_front() {
            Some(event) => Some(Ok(event)),
            None => queue.end.map(Err),
        }
    }

    fn queue(&self, operation: &'static str) -> Guard<'_, Queue<T>> {
        self.shared.world.check_unless_panicking();
        self.inbox.queue.lock(operation)
    }
}

impl<T> Clone for Fanout<T> {
    fn clone(&self) -> Self {
        let mut state = self.shared.lock("Fanout::clone");
        state.fanouts = state.fanouts.saturating_add(1);
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Fanout<T> {
    fn drop(&mut self) {
        if self.shared.world.unwinding_in_foreign_world() {
            return;
        }
        let mut state = self.shared.lock("Fanout::drop");
        state.fanouts = state.fanouts.saturating_sub(1);
        if state.fanouts == 0 {
            self.shared.close_with(state);
        }
    }
}

impl<T> Drop for Feed<T> {
    fn drop(&mut self) {
        if self.shared.world.unwinding_in_foreign_world() {
            return;
        }
        let mut state = self.shared.lock("Feed::drop");
        // The list's handle is never the last, since this feed holds one, so
        // removing it drops no event under the lock. The queued events drop
        // with `self.inbox`, after the lock is released.
        state.feeds.retain(|inbox| !Arc::ptr_eq(inbox, &self.inbox));
    }
}

impl<T> fmt::Debug for Fanout<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Try the lock to avoid deadlock when formatting from T::clone.
        self.shared.world.check();
        let Some(state) = self.shared.state.try_lock("Fanout::fmt") else {
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
        let queue = self.queue("Feed::fmt");
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
