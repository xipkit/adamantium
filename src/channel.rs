use std::fmt;

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
}

/// The receiving half of a bounded channel, created by [`Channel::split`].
///
/// Clones compete for messages: each message goes to exactly one receiver.
/// Dropping the last receiver closes the channel and wakes pending sends
/// with [`Closed`]. A combined [`Channel`] also owns a receiver and keeps
/// the receiving side alive.
pub struct Receiver<T> {
    inner: async_channel::Receiver<T>,
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

/// Returned by [`Receiver::try_receive`] when no message can be received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryReceiveError {
    /// The channel is empty but still open.
    Empty,
    /// The channel is closed and all queued messages have been received.
    Closed,
}

/// Returned by [`Sender::send`] or [`Channel::send`] when the channel is
/// closed. Holds the value that was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed<T>(pub T);

impl<T> Channel<T> {
    /// A channel that holds at most `capacity` messages. A capacity of zero
    /// is treated as one.
    pub fn bounded(capacity: usize) -> Self {
        let (sender, receiver) = async_channel::bounded(capacity.max(1));
        Self {
            sender: Sender { inner: sender },
            receiver: Receiver { inner: receiver },
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

    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained.
    pub async fn receive(&self) -> Option<T> {
        self.receiver.receive().await
    }

    /// Receives a value if one is waiting. Returns `None` when the channel
    /// is empty, whether or not it is closed. For distinct empty and closed
    /// outcomes, use [`Receiver::try_receive`] after [`split`](Self::split).
    pub fn try_receive(&self) -> Option<T> {
        self.receiver.try_receive().ok()
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
        self.inner
            .send(value)
            .await
            .map_err(|error| Closed(error.into_inner()))
    }

    /// Sends without waiting, returning the value if the channel is full
    /// or closed.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value).map_err(|error| match error {
            async_channel::TrySendError::Full(value) => TrySendError::Full(value),
            async_channel::TrySendError::Closed(value) => TrySendError::Closed(value),
        })
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers drain queued messages before returning `None`. Returns
    /// `false` if the channel was already closed.
    pub fn close(&self) -> bool {
        self.inner.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.inner.capacity().unwrap_or(usize::MAX)
    }
}

impl<T> Receiver<T> {
    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained, including after all
    /// senders have been dropped.
    pub async fn receive(&self) -> Option<T> {
        self.inner.recv().await.ok()
    }

    /// Receives without waiting. Unlike [`Channel::try_receive`], this
    /// distinguishes an empty open channel from a closed, drained channel.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        self.inner.try_recv().map_err(|error| match error {
            async_channel::TryRecvError::Empty => TryReceiveError::Empty,
            async_channel::TryRecvError::Closed => TryReceiveError::Closed,
        })
    }

    /// Closes the channel for every handle and wakes pending operations.
    /// Receivers can still drain queued messages. Returns `false` if the
    /// channel was already closed.
    pub fn close(&self) -> bool {
        self.inner.close()
    }

    /// Whether the channel is closed, possibly with messages still queued.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// The number of messages waiting.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether no messages are waiting.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The most messages the channel holds.
    pub fn capacity(&self) -> usize {
        self.inner.capacity().unwrap_or(usize::MAX)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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
