use std::fmt;

/// A bounded channel that any number of tasks can send on and receive from.
///
/// Every clone is both a sender and a receiver. `send` waits while the
/// channel is full, `receive` waits while it is empty, and each message
/// goes to exactly one receiver. Because every clone can send, the channel
/// doesn't close when senders go away: a producer calls
/// [`close`](Self::close) when it is done, after which receivers drain
/// what remains and then get `None`.
///
/// ```
/// # use adamantium::{Channel, Tasks};
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
/// ```
pub struct Channel<T> {
    sender: async_channel::Sender<T>,
    receiver: async_channel::Receiver<T>,
}

/// Returned by [`Channel::send`] when the channel is closed. Holds the
/// value that was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed<T>(pub T);

impl<T> Channel<T> {
    /// A channel that holds at most `capacity` messages. A capacity of zero
    /// is treated as one.
    pub fn bounded(capacity: usize) -> Self {
        let (sender, receiver) = async_channel::bounded(capacity.max(1));
        Self { sender, receiver }
    }

    /// Sends a value, waiting while the channel is full. Hands the value
    /// back if the channel is closed.
    pub async fn send(&self, value: T) -> Result<(), Closed<T>> {
        self.sender
            .send(value)
            .await
            .map_err(|error| Closed(error.into_inner()))
    }

    /// Receives the next value, waiting while the channel is empty. Returns
    /// `None` once the channel is closed and drained.
    pub async fn receive(&self) -> Option<T> {
        self.receiver.recv().await.ok()
    }

    /// Receives a value if one is waiting. Returns `None` when the channel
    /// is empty, whether or not it is closed.
    pub fn try_receive(&self) -> Option<T> {
        self.receiver.try_recv().ok()
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
        self.sender.capacity().unwrap_or(usize::MAX)
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
