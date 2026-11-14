use std::{fmt, sync::Arc};

#[cfg(feature = "tokio")]
use crate::runtime::wait::Waiter;
use crate::runtime::{Guard, Lock, World, queue::Bounded, wait::WaitQueue};
#[cfg(feature = "tokio")]
use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

#[cfg(feature = "tokio")]
use futures_core::Stream;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, shutdown};

/// A bounded channel that any number of tasks can send on and receive from.
///
/// Every clone is both a sender and a receiver. `send` waits while the
/// channel is full, `receive` waits while it is empty, and each message
/// goes to exactly one receiver. Because every clone can send, the channel
/// stays open while any combined handle remains. Call [`close`](Self::close)
/// explicitly, or consume every combined handle with [`split`](Self::split)
/// to let separate [`Sender`] and [`Receiver`] ownership govern closure.
/// Receivers drain queued messages before returning `None`.
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
/// Clones compete for messages: each message goes to exactly one receiver.
/// Dropping the last receiver closes the channel and wakes pending sends
/// with [`Closed`]. A combined [`Channel`] also owns a receiver and keeps
/// the receiving side alive.
///
/// With the `tokio` feature, receivers implement `futures_core::Stream`,
/// yielding queued messages until the channel is closed and drained.
/// Receivers can be used directly with `StreamExt::next`.
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

/// Returned by [`Sender::try_send`] or [`Channel::try_send`].
/// Both variants hold the value that was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The channel is full but still open.
    Full(T),
    /// The channel is closed.
    Closed(T),
}

/// Returned by [`Receiver::try_receive`] or [`Channel::try_receive`] when no
/// message can be received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryReceiveError {
    /// The channel is empty but still open.
    Empty,
    /// The channel is closed and all queued messages have been received.
    Closed,
}

/// Returned by [`Sender::send_until`] or [`Channel::send_until`]. Every
/// variant holds the value that was not sent.
#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError<T> {
    /// The channel is closed.
    Closed(T),
    /// The caller requested shutdown.
    Cancelled(T),
    /// The deadline elapsed.
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

/// Returned by [`Receiver::receive_until`] or [`Channel::receive_until`]
/// when no message was received.
#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveError {
    /// The channel is closed and all queued messages have been received.
    Closed,
    /// The caller requested shutdown.
    Cancelled,
    /// The deadline elapsed.
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
            items: WaitQueue::new(),
            space: WaitQueue::new(),
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
        self.sender.queue.world.step();
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
        let mut value = value;
        loop {
            value = match self.try_send(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(Closed(value)),
                Err(TrySendError::Full(value)) => value,
            };
            let waiter = self.queue.space.listen();
            // Registration precedes the recheck, so a receive cannot be lost.
            value = match self.try_send(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(Closed(value)),
                Err(TrySendError::Full(value)) => value,
            };
            waiter.await;
        }
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.queue.values.push(value).map_err(|error| match error {
            concurrent_queue::PushError::Full(value) => TrySendError::Full(value),
            concurrent_queue::PushError::Closed(value) => TrySendError::Closed(value),
        })?;
        self.queue.items.notify_one();
        Ok(())
    }

    /// Sends a value, waiting while the channel is full, until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline needs Tokio's time driver.
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
        let mut stop = pin!(shutdown(cancellation, deadline));
        let mut value = value;
        let mut space: Option<Waiter> = None;
        loop {
            let room = async {
                if let Some(space) = space.take() {
                    space.await;
                }
            };
            tokio::select! {
                biased;
                reason = &mut stop => {
                    return Err(match reason {
                        Shutdown::Cancelled => SendError::Cancelled(value),
                        Shutdown::DeadlineExceeded => SendError::DeadlineExceeded(value),
                    });
                }
                () = room => {}
            }
            space = Some(self.queue.space.listen());
            value = match self.try_send(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(value)) => return Err(SendError::Closed(value)),
                Err(TrySendError::Full(value)) => value,
            };
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
        self.queue.values.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.queue.values.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.queue.world.step();
        self.queue.capacity
    }
}

impl<T> Receiver<T> {
    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained, including after all
    /// senders have been dropped.
    pub async fn receive(&self) -> Option<T> {
        loop {
            match self.try_receive() {
                Ok(value) => return Some(value),
                Err(TryReceiveError::Closed) => return None,
                Err(TryReceiveError::Empty) => {}
            }
            let waiter = self.queue.items.listen();
            match self.try_receive() {
                Ok(value) => return Some(value),
                Err(TryReceiveError::Closed) => return None,
                Err(TryReceiveError::Empty) => {}
            }
            waiter.await;
        }
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the channel is open and empty, or [`TryReceiveError::Closed`] once
    /// it is closed and drained.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        let value = self.queue.values.pop().map_err(|error| match error {
            concurrent_queue::PopError::Empty => TryReceiveError::Empty,
            concurrent_queue::PopError::Closed => TryReceiveError::Closed,
        })?;
        self.queue.space.notify_one();
        Ok(value)
    }

    /// Receives the next value, waiting while the channel is empty, until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline needs Tokio's time driver.
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
        let reason = tokio::select! {
            biased;
            reason = shutdown(cancellation, deadline) => reason,
            received = self.receive() => return received.ok_or(ReceiveError::Closed),
        };
        Err(match reason {
            Shutdown::Cancelled => ReceiveError::Cancelled,
            Shutdown::DeadlineExceeded => ReceiveError::DeadlineExceeded,
        })
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers can still drain queued messages. Returns `false` if the
    /// channel was already closed.
    pub fn close(&self) -> bool {
        self.queue.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        self.queue.values.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.queue.values.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.queue.world.step();
        self.queue.capacity
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.queue.lock().senders += 1;
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
                this.waiter = Some(this.queue.items.listen());
            }
            // Recheck after registration or a consumed notification.
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.queue.lock().receivers += 1;
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
        self.queue.drop_endpoint(true);
    }
}

struct Queue<T> {
    state: Lock<State>,
    values: Bounded<T>,
    capacity: usize,
    items: WaitQueue,
    space: WaitQueue,
    world: World,
}

struct State {
    senders: usize,
    receivers: usize,
}

impl<T> Queue<T> {
    fn lock(&self) -> Guard<'_, State> {
        self.world.step();
        self.state.lock()
    }

    fn close(&self) -> bool {
        let was_open = self.values.close();
        if was_open {
            self.wake_closed();
        }
        was_open
    }

    fn wake_closed(&self) {
        self.items.notify_all();
        self.space.notify_all();
    }

    fn drop_endpoint(&self, sender: bool) {
        #[cfg(feature = "deterministic")]
        if std::thread::panicking() && self.world != World::current() {
            return;
        }
        self.world.step_unless_panicking();
        let mut state = self.state.lock();
        let count = if sender {
            &mut state.senders
        } else {
            &mut state.receivers
        };
        *count -= 1;
        let last = *count == 0;
        drop(state);
        if last && self.values.close() {
            self.wake_closed();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.queue.drop_endpoint(false);
    }
}
