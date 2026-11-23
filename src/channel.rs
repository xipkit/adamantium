use std::{fmt, sync::Arc};

#[cfg(feature = "tokio")]
use crate::runtime::wait::Waiter;
use crate::runtime::{Guard, Lock, World, queue::Bounded, wait::WaitQueue};
#[cfg(feature = "tokio")]
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

#[cfg(feature = "tokio")]
use futures_core::Stream;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, before_shutdown};

/// A bounded channel with any number of senders and receivers.
///
/// Each message goes to one receiver. Sends wait for space and receives
/// wait for a message. Each clone keeps both sides open. Call [`Self::close`]
/// explicitly or [`Self::split`] every combined handle to let ownership
/// govern closure. Receivers drain queued messages before returning `None`.
///
/// ```
/// # #[cfg(feature = "tokio")]
/// # use adamantium::{Channel, Tasks};
/// # #[cfg(feature = "tokio")]
/// # #[tokio::main] async fn main() -> Result<(), &'static str> {
/// let numbers = Channel::<u64>::bounded(4);
/// let total = Tasks::<&str>::run(async |t| {
///     let producer = numbers.clone();
///     t.spawn(async move {
///         for n in 1..=10 {
///             producer.send(n).await.map_err(|_| "closed")?;
///         }
///         producer.close();
///         Ok(())
///     });
///     let consumer = numbers.clone();
///     let sum = t.spawn(async move {
///         let mut sum = 0;
///         while let Some(n) = consumer.receive().await {
///             sum += n;
///         }
///         Ok(sum)
///     });
///     t.value(sum).await
/// })
/// .await?;
/// assert_eq!(total, 55);
/// # Ok(()) }
/// # #[cfg(not(feature = "tokio"))]
/// # fn main() {}
/// ```
pub struct Channel<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

/// The sending half of a bounded channel, created by [`Channel::split`].
///
/// Clones share the same queue. Dropping the last sender closes the channel;
/// receivers can drain queued messages before receiving `None`. A combined
/// [`Channel`] also owns a sender and keeps the sending side alive.
pub struct Sender<T> {
    queue: Arc<Queue<T>>,
}

/// The receiving half of a bounded channel, created by [`Channel::split`].
///
/// Each message goes to one receiver. Dropping the last receiver closes the
/// channel, wakes pending sends with [`Closed`], and drops queued messages.
/// A combined [`Channel`] also keeps the receiving side alive.
///
/// With `tokio`, receivers implement `Stream` and yield until the channel
/// closes and drains.
///
/// ```
/// # #[cfg(feature = "tokio")]
/// # #[tokio::main] async fn main() {
/// use adamantium::Channel;
/// use futures_util::StreamExt;
///
/// let (sender, receiver) = Channel::bounded(2).split();
/// sender.send(1).await.unwrap();
/// sender.send(2).await.unwrap();
/// drop(sender);
///
/// let mut receiver = std::pin::pin!(receiver);
/// assert_eq!(receiver.next().await, Some(1));
/// assert_eq!(receiver.next().await, Some(2));
/// assert_eq!(receiver.next().await, None);
/// # }
/// # #[cfg(not(feature = "tokio"))]
/// # fn main() {}
/// ```
pub struct Receiver<T> {
    queue: Arc<Queue<T>>,
    #[cfg(feature = "tokio")]
    waiter: Option<Waiter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryReceiveError {
    Empty,
    Closed,
}

#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError<T> {
    Closed(T),
    Cancelled(T),
    DeadlineExceeded(T),
}

#[cfg(feature = "tokio")]
impl<T> SendError<T> {
    /// The value that was not sent.
    pub fn into_inner(self) -> T {
        match self {
            Self::Closed(value) | Self::Cancelled(value) | Self::DeadlineExceeded(value) => value,
        }
    }
}

#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveError {
    Closed,
    Cancelled,
    DeadlineExceeded,
}

/// Returned by [`Sender::send`] or [`Channel::send`] when the channel is
/// closed. Holds the value that was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed<T>(pub T);

impl<T> Channel<T> {
    /// A channel that holds at most `capacity` messages. A capacity of zero
    /// is treated as one.
    pub fn bounded(capacity: usize) -> Self {
        let queue = Arc::new(Queue {
            state: Lock::new(State {
                senders: 1,
                receivers: 1,
            }),
            values: Bounded::new(capacity.max(1)),
            capacity: capacity.max(1),
            items: WaitQueue::named("channel.receive"),
            #[cfg(feature = "tokio")]
            streams: WaitQueue::named("channel.stream"),
            space: WaitQueue::named("channel.send"),
            world: World::current(),
        });
        Self {
            sender: Sender {
                queue: Arc::clone(&queue),
            },
            receiver: Receiver {
                queue,
                #[cfg(feature = "tokio")]
                waiter: None,
            },
        }
    }

    /// Consumes this combined handle and returns its separate endpoints.
    ///
    /// Any earlier `Channel` clones still own both endpoints. Drop or split
    /// all combined handles before relying on endpoint drops to close the
    /// channel.
    ///
    /// ```
    /// use adamantium::{Channel, TryReceiveError};
    ///
    /// let (sender, receiver) = Channel::bounded(2).split();
    /// let producer = sender.clone();
    /// sender.try_send(1).unwrap();
    /// producer.try_send(2).unwrap();
    /// drop(sender);
    /// drop(producer);
    /// assert_eq!(receiver.try_receive(), Ok(1));
    /// assert_eq!(receiver.try_receive(), Ok(2));
    /// assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));
    /// ```
    pub fn split(self) -> (Sender<T>, Receiver<T>) {
        self.sender.queue.world.step("Channel::split");
        (self.sender, self.receiver)
    }

    /// Sends a value, waiting while the channel is full. Hands the value
    /// back if the channel is closed.
    pub async fn send(&self, value: T) -> Result<(), Closed<T>> {
        self.sender.send(value).await
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(value)
    }

    /// Sends a value, waiting while the channel is full, until
    /// `cancellation` is requested or `deadline` passes. Every failure hands
    /// the value back. See [`Sender::send_until`].
    #[cfg(feature = "tokio")]
    pub async fn send_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        value: T,
    ) -> Result<(), SendError<T>> {
        self.sender.send_until(cancellation, deadline, value).await
    }

    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained.
    pub async fn receive(&self) -> Option<T> {
        self.receiver.receive().await
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the channel is open and empty, or [`TryReceiveError::Closed`] once
    /// it is closed and drained.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        self.receiver.try_receive()
    }

    /// Receives up to `max` queued values without waiting. See
    /// [`Receiver::try_receive_batch`].
    pub fn try_receive_batch(&self, max: usize) -> Result<Vec<T>, TryReceiveError> {
        self.receiver.try_receive_batch(max)
    }

    /// Receives the next value, waiting while the channel is empty, until
    /// `cancellation` is requested or `deadline` passes. See
    /// [`Receiver::receive_until`].
    #[cfg(feature = "tokio")]
    pub async fn receive_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<T, ReceiveError> {
        self.receiver.receive_until(cancellation, deadline).await
    }

    /// Appends up to `max` values to `batch`, waiting at most `linger` after
    /// the first value. See [`Receiver::receive_batch_until`].
    #[cfg(feature = "tokio")]
    pub async fn receive_batch_until<'a>(
        &self,
        cancellation: &Cancellation,
        max: usize,
        linger: Duration,
        batch: &'a mut Vec<T>,
    ) -> Result<&'a [T], ReceiveError> {
        self.receiver
            .receive_batch_until(cancellation, max, linger, batch)
            .await
    }

    /// Closes the channel. Sends fail from now on; receives drain what is
    /// queued and then return `None`. Returns `false` if it was already
    /// closed.
    pub fn close(&self) -> bool {
        self.sender.close()
    }

    /// Whether the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.sender.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.sender.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
        }
    }
}

impl<T> fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<T: fmt::Debug> fmt::Display for Closed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the channel is closed; could not send {:?}", self.0)
    }
}

impl<T: fmt::Debug> std::error::Error for Closed<T> {}

impl<T> Sender<T> {
    /// Sends a value, waiting while the channel is full. Returns the value
    /// in [`Closed`] if the channel is closed, including when all receivers
    /// have been dropped.
    pub async fn send(&self, value: T) -> Result<(), Closed<T>> {
        self.send_value(&mut Some(value)).await
    }

    // A cancelled send_until can recover the value while it is waiting for room.
    #[allow(
        clippy::expect_used,
        reason = "a full queue always returns the pending value"
    )]
    async fn send_value(&self, value: &mut Option<T>) -> Result<(), Closed<T>> {
        self.queue
            .space
            .wait_for(|| {
                let pending = value.take().expect("send still owns its value");
                match self.try_send(pending) {
                    Ok(()) => Some(Ok(())),
                    Err(TrySendError::Closed(unsent)) => Some(Err(Closed(unsent))),
                    Err(TrySendError::Full(unsent)) => {
                        *value = Some(unsent);
                        None
                    }
                }
            })
            .await
    }

    pub(crate) async fn room(&self) {
        loop {
            if self.has_room() {
                return;
            }
            let waiter = self.queue.space.listen();
            // Register before rechecking to avoid losing a wakeup.
            if self.has_room() {
                return;
            }
            waiter.await;
        }
    }

    fn has_room(&self) -> bool {
        self.is_closed() || self.len() < self.capacity()
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.queue
            .values
            .push(value, "Channel::try_send")
            .map_err(|error| match error {
                concurrent_queue::PushError::Full(value) => TrySendError::Full(value),
                concurrent_queue::PushError::Closed(value) => TrySendError::Closed(value),
            })?;
        self.queue.items.notify_one();
        #[cfg(feature = "tokio")]
        self.queue.streams.notify_all();
        Ok(())
    }

    /// Sends a value, waiting while the channel is full, until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline uses [`crate::time`].
    ///
    /// Every failure hands the value back in [`SendError`]. A request
    /// already present when the call is made, or at any later poll, wins
    /// over room in the channel, and cancellation wins over an equally ready
    /// deadline, as in [`Tasks::run_until`](crate::Tasks::run_until). The
    /// value enters the queue only when the send succeeds, so dropping the
    /// returned future drops the value without sending it.
    ///
    /// ```
    /// use adamantium::{Cancellation, Channel, SendError, time};
    /// use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let (sender, receiver) = Channel::bounded(1).split();
    /// let cancellation = Cancellation::new();
    /// sender.send_until(&cancellation, None, 1).await.unwrap();
    /// let soon = Some(time::now() + Duration::from_millis(10));
    /// assert_eq!(
    ///     sender.send_until(&cancellation, soon, 2).await,
    ///     Err(SendError::DeadlineExceeded(2)),
    /// );
    /// assert_eq!(receiver.try_receive(), Ok(1));
    /// # }
    /// ```
    #[cfg(feature = "tokio")]
    pub async fn send_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        value: T,
    ) -> Result<(), SendError<T>> {
        self.queue.world.check();
        let mut value = Some(value);
        match crate::until(cancellation, deadline, self.send_value(&mut value)).await {
            Ok(sent) => sent.map_err(|Closed(value)| SendError::Closed(value)),
            Err(reason) => {
                #[allow(clippy::expect_used, reason = "only a completed send takes the value")]
                let value = value.expect("shutdown won before send completed");
                Err(match reason {
                    crate::UntilError::Cancelled => SendError::Cancelled(value),
                    crate::UntilError::DeadlineExceeded => SendError::DeadlineExceeded(value),
                })
            }
        }
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers drain queued messages before returning `None`. Returns
    /// `false` if the channel was already closed.
    pub fn close(&self) -> bool {
        self.queue.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        self.queue.values.is_closed("Channel::is_closed")
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.queue.values.len("Channel::len")
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.queue.world.step("Channel::capacity");
        self.queue.capacity
    }
}

impl<T> Receiver<T> {
    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained, including after all
    /// senders have been dropped.
    pub async fn receive(&self) -> Option<T> {
        self.queue
            .items
            .wait_for(|| match self.try_receive() {
                Ok(value) => Some(Some(value)),
                Err(TryReceiveError::Closed) => Some(None),
                Err(TryReceiveError::Empty) => None,
            })
            .await
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the channel is open and empty, or [`TryReceiveError::Closed`] once
    /// it is closed and drained.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        let value = self
            .queue
            .values
            .pop("Channel::try_receive")
            .map_err(|error| match error {
                concurrent_queue::PopError::Empty => TryReceiveError::Empty,
                concurrent_queue::PopError::Closed => TryReceiveError::Closed,
            })?;
        self.queue.space.notify_one();
        Ok(value)
    }

    /// Receives up to `max` queued values without waiting. Returns the
    /// values received in queue order, even if fewer than `max` were ready.
    /// With no values ready, returns [`TryReceiveError::Empty`] while open
    /// or [`TryReceiveError::Closed`] after closure. A `max` of zero returns
    /// an empty batch immediately.
    pub fn try_receive_batch(&self, max: usize) -> Result<Vec<T>, TryReceiveError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut batch = vec![self.try_receive()?];
        while batch.len() < max {
            let Ok(value) = self.try_receive() else {
                break;
            };
            batch.push(value);
        }
        Ok(batch)
    }

    /// Receives the next value, waiting while the channel is empty, until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline uses [`crate::time`].
    ///
    /// A request already present when the call is made, or at any later
    /// poll, wins over a queued message, and cancellation wins over an
    /// equally ready deadline, as in [`Tasks::run_until`](crate::Tasks::run_until).
    /// A message is only taken from the queue when it is returned, so ending
    /// early, or dropping the returned future, loses nothing.
    ///
    /// ```
    /// use adamantium::{Cancellation, Channel, ReceiveError, time};
    /// use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let (sender, receiver) = Channel::<u32>::bounded(1).split();
    /// let cancellation = Cancellation::new();
    /// let soon = Some(time::now() + Duration::from_millis(10));
    /// assert_eq!(
    ///     receiver.receive_until(&cancellation, soon).await,
    ///     Err(ReceiveError::DeadlineExceeded),
    /// );
    /// sender.try_send(7).unwrap();
    /// assert_eq!(receiver.receive_until(&cancellation, None).await, Ok(7));
    /// # }
    /// ```
    #[cfg(feature = "tokio")]
    pub async fn receive_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<T, ReceiveError> {
        self.queue.world.check();
        before_shutdown(cancellation, deadline, self.receive())
            .await
            .map_err(Shutdown::receive_error)?
            .ok_or(ReceiveError::Closed)
    }

    /// Appends up to `max` values to `batch` and returns the newly appended slice.
    /// The first value has no deadline. After it arrives, `linger` sets one
    /// deadline for the rest of the batch using [`crate::time`]. A zero linger
    /// returns after the first value, and a zero `max` returns an empty slice
    /// immediately.
    ///
    /// When the linger expires, the channel has closed and drained, or
    /// `cancellation` is requested, returns any values appended by this call.
    /// Before the first value, cancellation returns [`ReceiveError::Cancelled`]
    /// and a closed and drained channel returns [`ReceiveError::Closed`].
    /// A cancellation that is already requested at a poll takes precedence.
    /// Dropping the future leaves any values it has received in `batch`.
    #[cfg(feature = "tokio")]
    pub async fn receive_batch_until<'a>(
        &self,
        cancellation: &Cancellation,
        max: usize,
        linger: Duration,
        batch: &'a mut Vec<T>,
    ) -> Result<&'a [T], ReceiveError> {
        let start = batch.len();
        if max > 0 {
            batch.push(self.receive_until(cancellation, None).await?);
            let deadline = crate::time::add_capped(crate::time::now(), linger);
            for _ in 1..max {
                let Ok(value) = self.receive_until(cancellation, Some(deadline)).await else {
                    break;
                };
                batch.push(value);
            }
        }
        Ok(batch.get(start..).unwrap_or_default())
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers can still drain queued messages. Returns `false` if the
    /// channel was already closed.
    pub fn close(&self) -> bool {
        self.queue.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        self.queue.values.is_closed("Channel::is_closed")
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.queue.values.len("Channel::len")
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.queue.world.step("Channel::capacity");
        self.queue.capacity
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.queue.lock("Sender::clone").senders += 1;
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

#[cfg(feature = "tokio")]
impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use std::future::Future;
        self.queue.world.poll_cooperative(cx, |cx| {
            loop {
                match self.try_receive() {
                    Ok(value) => {
                        self.as_mut().get_mut().waiter = None;
                        return Poll::Ready(Some(value));
                    }
                    Err(TryReceiveError::Closed) => {
                        self.as_mut().get_mut().waiter = None;
                        return Poll::Ready(None);
                    }
                    Err(TryReceiveError::Empty) => {}
                }
                let this = self.as_mut().get_mut();
                if let Some(waiter) = &mut this.waiter {
                    if Pin::new(waiter).poll(cx).is_pending() {
                        return Poll::Pending;
                    }
                    this.waiter = None;
                } else {
                    this.waiter = Some(this.queue.streams.listen());
                }
                // Recheck after registration or a consumed notification.
            }
        })
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.queue.lock("Receiver::clone").receivers += 1;
        Self {
            queue: Arc::clone(&self.queue),
            #[cfg(feature = "tokio")]
            waiter: None,
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<T: fmt::Debug> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(value) => write!(f, "the channel is full; could not send {value:?}"),
            Self::Closed(value) => write!(f, "the channel is closed; could not send {value:?}"),
        }
    }
}

impl<T: fmt::Debug> std::error::Error for TrySendError<T> {}

impl fmt::Display for TryReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the channel is empty"),
            Self::Closed => f.write_str("the channel is closed and empty"),
        }
    }
}

impl std::error::Error for TryReceiveError {}

#[cfg(feature = "tokio")]
impl<T: fmt::Debug> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(value) => write!(f, "the channel is closed; could not send {value:?}"),
            Self::Cancelled(value) => write!(f, "send cancelled; could not send {value:?}"),
            Self::DeadlineExceeded(value) => {
                write!(f, "send deadline exceeded; could not send {value:?}")
            }
        }
    }
}

#[cfg(feature = "tokio")]
impl<T: fmt::Debug> std::error::Error for SendError<T> {}

#[cfg(feature = "tokio")]
impl fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the channel is closed and empty"),
            Self::Cancelled => f.write_str("receive cancelled"),
            Self::DeadlineExceeded => f.write_str("receive deadline exceeded"),
        }
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for ReceiveError {}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.queue.drop_endpoint(true, "Sender::drop");
    }
}

struct Queue<T> {
    state: Lock<State>,
    values: Bounded<T>,
    capacity: usize,
    items: WaitQueue,
    // Next cancellation leaves its listener on the receiver. Wake every stream
    // separately so an idle receiver cannot consume another operation's wakeup.
    #[cfg(feature = "tokio")]
    streams: WaitQueue,
    space: WaitQueue,
    world: World,
}

struct State {
    senders: usize,
    receivers: usize,
}

impl<T> Queue<T> {
    fn lock(&self, operation: &'static str) -> Guard<'_, State> {
        self.world.step(operation);
        self.state.lock(operation)
    }

    fn close(&self) -> bool {
        let was_open = self.values.close("Channel::close");
        if was_open {
            self.wake_closed();
        }
        was_open
    }

    fn wake_closed(&self) {
        self.items.notify_all();
        #[cfg(feature = "tokio")]
        self.streams.notify_all();
        self.space.notify_all();
    }

    fn drop_endpoint(&self, sender: bool, operation: &'static str) {
        #[cfg(feature = "deterministic")]
        if std::thread::panicking() && self.world != World::current() {
            return;
        }
        self.world.step_unless_panicking(operation);
        let mut state = self.state.lock(operation);
        let count = if sender {
            &mut state.senders
        } else {
            &mut state.receivers
        };
        *count -= 1;
        let last = *count == 0;
        drop(state);
        if !last {
            return;
        }
        if self.values.close(operation) {
            self.wake_closed();
        }
        if !sender {
            // Wake waiters before dropping messages outside the lock. A destructor
            // can use this channel and must see it closed without deadlocking.
            self.values.clear(operation);
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.queue.drop_endpoint(false, "Receiver::drop");
    }
}
