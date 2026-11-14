use std::{fmt, sync::Arc};
#[cfg(feature = "tokio")]
use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use event_listener::Event;
#[cfg(feature = "tokio")]
use event_listener::EventListener;
#[cfg(feature = "tokio")]
use futures_core::Stream;
use pin_project_lite::pin_project;
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
    inner: async_channel::Sender<T>,
    space: Arc<Event>,
}

pin_project! {
    /// The receiving half of a bounded channel, created by [`Channel::split`].
    ///
    /// Clones compete for messages: each message goes to exactly one receiver.
    /// Dropping the last receiver closes the channel and wakes pending sends
    /// with [`Closed`]. A combined [`Channel`] also owns a receiver and keeps
    /// the receiving side alive.
    ///
    /// With the `tokio` feature, receivers implement `futures_core::Stream`,
    /// yielding queued messages until the channel is closed and drained.
    /// Pin the receiver before using `StreamExt::next`, as it is not `Unpin`.
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
        #[pin]
        inner: async_channel::Receiver<T>,
        space: Arc<Event>,
    }

    impl<T> PinnedDrop for Receiver<T> {
        fn drop(_this: Pin<&mut Self>) {
            #[cfg(feature = "deterministic")]
            if !std::thread::panicking() {
                crate::runtime::production_only("Channel");
            }
        }
    }
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
        crate::runtime::production_only("Channel");
        let (sender, receiver) = async_channel::bounded(capacity.max(1));
        // A pending async_channel send owns its value and drops it when the
        // send is cancelled, so `send_until` waits for room on this event
        // instead and only gives the value up to `try_send`. Every receive
        // notifies it.
        let space = Arc::new(Event::new());
        Self {
            sender: Sender {
                inner: sender,
                space: Arc::clone(&space),
            },
            receiver: Receiver {
                inner: receiver,
                space,
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
        crate::runtime::production_only("Channel");
        (self.sender, self.receiver)
    }

    /// Sends a value, waiting while the channel is full. Hands the value
    /// back if the channel is closed.
    pub async fn send(&self, value: T) -> Result<(), Closed<T>> {
        crate::runtime::production_future("Channel", async move { self.sender.send(value).await })
            .await
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        crate::runtime::production_only("Channel");
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
        crate::runtime::production_future("Channel", async move {
            self.sender.send_until(cancellation, deadline, value).await
        })
        .await
    }

    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained.
    pub async fn receive(&self) -> Option<T> {
        crate::runtime::production_future("Channel", async move { self.receiver.receive().await })
            .await
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the channel is open and empty, or [`TryReceiveError::Closed`] once
    /// it is closed and drained.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        crate::runtime::production_only("Channel");
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
        crate::runtime::production_future("Channel", async move {
            self.receiver.receive_until(cancellation, deadline).await
        })
        .await
    }

    /// Closes the channel. Sends fail from now on; receives drain what is
    /// queued and then return `None`. Returns `false` if it was already
    /// closed.
    pub fn close(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.sender.close()
    }

    /// Whether the channel is closed.
    pub fn is_closed(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.sender.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.sender.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.sender.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.sender.capacity()
    }
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        crate::runtime::production_only("Channel");
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
        crate::runtime::production_future("Channel", async move {
            self.inner
                .send(value)
                .await
                .map_err(|error| Closed(error.into_inner()))
        })
        .await
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        crate::runtime::production_only("Channel");
        self.inner.try_send(value).map_err(|error| match error {
            async_channel::TrySendError::Full(value) => TrySendError::Full(value),
            async_channel::TrySendError::Closed(value) => TrySendError::Closed(value),
        })
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
        crate::runtime::production_future("Channel", async move {
            let mut stop = pin!(shutdown(cancellation, deadline));
            let mut value = value;
            let mut space: Option<EventListener> = None;
            loop {
                // The first pass tries at once; later passes wait for a receive
                // to make room or for the channel to close.
                let room = async {
                    if let Some(space) = space.take() {
                        tokio::select! {
                            () = space => {}
                            () = self.inner.closed() => {}
                        }
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
                // Listen before trying, so room made after a failed try still
                // wakes this send.
                space = Some(self.space.listen());
                value = match self.try_send(value) {
                    Ok(()) => return Ok(()),
                    Err(TrySendError::Closed(value)) => return Err(SendError::Closed(value)),
                    Err(TrySendError::Full(value)) => value,
                };
            }
        })
        .await
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers drain queued messages before returning `None`. Returns
    /// `false` if the channel was already closed.
    pub fn close(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.inner.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.inner.capacity().unwrap_or(usize::MAX)
    }
}

impl<T> Receiver<T> {
    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained, including after all
    /// senders have been dropped.
    pub async fn receive(&self) -> Option<T> {
        crate::runtime::production_future("Channel", async move {
            self.inner
                .recv()
                .await
                .ok()
                .map(|value| self.received(value))
        })
        .await
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the channel is open and empty, or [`TryReceiveError::Closed`] once
    /// it is closed and drained.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        crate::runtime::production_only("Channel");
        self.inner
            .try_recv()
            .map(|value| self.received(value))
            .map_err(|error| match error {
                async_channel::TryRecvError::Empty => TryReceiveError::Empty,
                async_channel::TryRecvError::Closed => TryReceiveError::Closed,
            })
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
        crate::runtime::production_future("Channel", async move {
            let reason = tokio::select! {
                biased;
                reason = shutdown(cancellation, deadline) => reason,
                received = self.inner.recv() => {
                    return received
                        .map(|value| self.received(value))
                        .map_err(|_| ReceiveError::Closed);
                }
            };
            Err(match reason {
                Shutdown::Cancelled => ReceiveError::Cancelled,
                Shutdown::DeadlineExceeded => ReceiveError::DeadlineExceeded,
            })
        })
        .await
    }

    /// Wakes one sender waiting in `send_until` for the place a received
    /// value left. A woken sender that is dropped passes the wake on.
    fn received(&self, value: T) -> T {
        self.space.notify_additional(1);
        value
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers can still drain queued messages. Returns `false` if the
    /// channel was already closed.
    pub fn close(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.inner.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        crate::runtime::production_only("Channel");
        self.inner.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        crate::runtime::production_only("Channel");
        self.inner.capacity().unwrap_or(usize::MAX)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        crate::runtime::production_only("Channel");
        Self {
            inner: self.inner.clone(),
            space: Arc::clone(&self.space),
        }
    }
}

#[cfg(feature = "tokio")]
impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        crate::runtime::production_only("Channel receiver stream");
        match self.as_mut().project().inner.poll_next(cx) {
            Poll::Ready(Some(value)) => Poll::Ready(Some(self.received(value))),
            poll => poll,
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        crate::runtime::production_only("Channel");
        Self {
            inner: self.inner.clone(),
            space: Arc::clone(&self.space),
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

#[cfg(feature = "deterministic")]
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::runtime::production_only("Channel");
        }
    }
}
