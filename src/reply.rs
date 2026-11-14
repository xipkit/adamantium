use std::fmt;

#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::ReceiveError;
#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, shutdown};
use crate::{Closed, TryReceiveError};

/// The sending half of a one-shot reply: a single value from one task to
/// another, such as the answer to a request.
///
/// [`Reply::pair`] makes a `Reply` and the [`ReplyReceiver`] that waits for
/// it. [`send`](Self::send) consumes the reply, so it can be sent at most
/// once. Dropping it without sending ends the receiver's wait with `None`.
///
/// ```
/// # #[cfg(feature = "tokio")]
/// # #[tokio::main] async fn main() -> Result<(), &'static str> {
/// use adamantium::{Channel, Reply, Tasks};
///
/// struct Lookup {
///     key: u32,
///     reply: Reply<String>,
/// }
///
/// let requests = Channel::<Lookup>::bounded(8);
/// let name = Tasks::<&str>::run(async |t| {
///     let server = requests.clone();
///     t.spawn(async move {
///         while let Some(Lookup { key, reply }) = server.receive().await {
///             // The asker may have given up; that is not the server's error.
///             drop(reply.send(format!("user {key}")));
///         }
///         Ok(())
///     });
///     let (reply, answer) = Reply::pair();
///     requests.send(Lookup { key: 7, reply }).await.map_err(|_| "closed")?;
///     let name = answer.receive().await.ok_or("no reply")?;
///     requests.close();
///     Ok(name)
/// })
/// .await?;
/// assert_eq!(name, "user 7");
/// # Ok(()) }
/// # #[cfg(not(feature = "tokio"))]
/// # fn main() {}
/// ```
pub struct Reply<T> {
    #[cfg(feature = "deterministic")]
    _runtime: crate::runtime::execution::Production,
    inner: async_channel::Sender<T>,
}

/// The receiving half of a one-shot reply, created by [`Reply::pair`].
///
/// It receives the one value its [`Reply`] sends, or `None` if the reply is
/// dropped without sending. Dropping the receiver tells the replying side
/// that nobody is waiting: its [`send`](Reply::send) hands the value back
/// and [`closed`](Reply::closed) completes.
pub struct ReplyReceiver<T> {
    #[cfg(feature = "deterministic")]
    _runtime: crate::runtime::execution::Production,
    inner: async_channel::Receiver<T>,
}

impl<T> Reply<T> {
    /// A reply and the receiver that waits for it.
    pub fn pair() -> (Self, ReplyReceiver<T>) {
        crate::runtime::production_only("Reply");
        let (sender, receiver) = async_channel::bounded(1);
        (
            Self {
                #[cfg(feature = "deterministic")]
                _runtime: crate::runtime::execution::Production("Reply"),
                inner: sender,
            },
            ReplyReceiver {
                #[cfg(feature = "deterministic")]
                _runtime: crate::runtime::execution::Production("Reply"),
                inner: receiver,
            },
        )
    }

    /// Sends the reply without waiting. Hands the value back in [`Closed`]
    /// if the receiver was dropped or closed.
    pub fn send(self, value: T) -> Result<(), Closed<T>> {
        crate::runtime::production_only("Reply");
        // The queue holds one value and this is the only send, so it is
        // never full.
        self.inner
            .try_send(value)
            .map_err(|error| Closed(error.into_inner()))
    }

    /// Whether the receiver was dropped or closed, so a reply would not be
    /// received.
    pub fn is_closed(&self) -> bool {
        crate::runtime::production_only("Reply");
        self.inner.is_closed()
    }

    /// Waits until the receiver is dropped or closed, so work whose answer
    /// nobody wants can stop early.
    pub async fn closed(&self) {
        crate::runtime::production_future("Reply", self.inner.closed()).await;
    }
}

impl<T> ReplyReceiver<T> {
    /// Waits for the reply. Returns `None` if the reply was dropped without
    /// sending, or once the value has already been received.
    ///
    /// Dropping the returned future loses nothing: a reply sent meanwhile
    /// stays for the next receive.
    pub async fn receive(&self) -> Option<T> {
        crate::runtime::production_future("Reply", self.inner.recv())
            .await
            .ok()
    }

    /// Receives without waiting. Returns [`TryReceiveError::Empty`] while
    /// the reply may still come, or [`TryReceiveError::Closed`] once it was
    /// dropped without sending, the receiver was closed, or the value was
    /// already received.
    pub fn try_receive(&self) -> Result<T, TryReceiveError> {
        crate::runtime::production_only("Reply");
        self.inner.try_recv().map_err(|error| match error {
            async_channel::TryRecvError::Empty => TryReceiveError::Empty,
            async_channel::TryRecvError::Closed => TryReceiveError::Closed,
        })
    }

    /// Waits for the reply until `cancellation` is requested or the
    /// absolute `deadline` passes. Pass `None` for no deadline. A deadline
    /// needs Tokio's time driver.
    ///
    /// A request already present when the call is made, or at any later
    /// poll, wins over a sent reply, and cancellation wins over an equally
    /// ready deadline, as in [`Tasks::run_until`](crate::Tasks::run_until).
    /// A reply left unreceived by ending early stays for the next receive.
    ///
    /// ```
    /// use adamantium::{Cancellation, ReceiveError, Reply};
    /// use tokio::time::{Duration, Instant};
    /// # #[tokio::main] async fn main() {
    /// let (reply, answer) = Reply::<u32>::pair();
    /// let cancellation = Cancellation::new();
    /// let soon = Some(Instant::now() + Duration::from_millis(10));
    /// assert_eq!(
    ///     answer.receive_until(&cancellation, soon).await,
    ///     Err(ReceiveError::DeadlineExceeded),
    /// );
    /// reply.send(7).unwrap();
    /// assert_eq!(answer.receive_until(&cancellation, None).await, Ok(7));
    /// # }
    /// ```
    #[cfg(feature = "tokio")]
    pub async fn receive_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<T, ReceiveError> {
        crate::runtime::production_future("Reply", async move {
            let reason = tokio::select! {
                biased;
                reason = shutdown(cancellation, deadline) => reason,
                received = self.inner.recv() => return received.map_err(|_| ReceiveError::Closed),
            };
            Err(match reason {
                Shutdown::Cancelled => ReceiveError::Cancelled,
                Shutdown::DeadlineExceeded => ReceiveError::DeadlineExceeded,
            })
        })
        .await
    }

    /// Tells the replying side that nobody is waiting, so its `send` hands
    /// the value back. A reply already sent can still be received. Returns
    /// `false` if the reply was already closed, sent, or dropped.
    pub fn close(&self) -> bool {
        crate::runtime::production_only("Reply");
        self.inner.close()
    }

    /// Whether no reply can arrive any more: it was sent, dropped, or
    /// closed. A sent reply may still be waiting to be received.
    pub fn is_closed(&self) -> bool {
        crate::runtime::production_only("Reply");
        self.inner.is_closed()
    }

    /// Whether a sent reply is waiting to be received.
    pub fn is_ready(&self) -> bool {
        crate::runtime::production_only("Reply");
        !self.inner.is_empty()
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<T> fmt::Debug for ReplyReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyReceiver")
            .field("ready", &self.is_ready())
            .field("closed", &self.is_closed())
            .finish()
    }
}
