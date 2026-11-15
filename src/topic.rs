//! Sending each message to every subscriber.

use std::{fmt, sync::Arc};

use crate::{
    channel::{Channel, Receiver, Sender, TrySendError},
    runtime::Lock,
};

#[cfg(feature = "tokio")]
use crate::{
    channel::SendError,
    race,
    tasks::{Cancellation, Shutdown, shutdown},
};
#[cfg(feature = "tokio")]
use std::pin::pin;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Sends each message to every subscriber, and drops none.
///
/// Each subscriber has its own bounded queue of `capacity` messages.
/// [`send`](Self::send) waits until every subscriber has room, then gives
/// each of them a clone of the message. A slow subscriber therefore slows
/// the topic down; no subscriber misses a message, and every subscriber
/// receives the messages in the order they were sent.
///
/// A topic has one sender. `send` takes `&mut self`, so two sends cannot
/// overlap and reach subscribers in different orders. To publish from
/// several tasks, send to one task that owns the topic through a
/// [`Channel`].
///
/// A subscriber receives the messages sent after it subscribed. Dropping its
/// [`Receiver`] unsubscribes it. Dropping the topic closes every subscriber,
/// which receives the messages already queued and then `None`.
///
/// ```
/// use adamantium::Topic;
///
/// # async fn example() {
/// let mut prices = Topic::new(8);
/// let display = prices.subscribe();
/// let log = prices.subscribe();
///
/// assert_eq!(prices.send(42).await, 2);
/// assert_eq!(display.receive().await, Some(42));
/// assert_eq!(log.receive().await, Some(42));
/// # }
/// ```
pub struct Topic<T> {
    subscriptions: Subscriptions<T>,
}

/// Adds subscribers to a [`Topic`] from other tasks. Get one from
/// [`Topic::subscriptions`]; clones add to the same topic.
pub struct Subscriptions<T> {
    subscribers: Arc<Lock<Subscribers<T>>>,
    capacity: usize,
}

struct Subscribers<T> {
    senders: Vec<Sender<T>>,
    open: bool,
}

impl<T> Topic<T> {
    /// A topic whose subscribers each queue at most `capacity` messages. A
    /// capacity of zero is treated as one.
    pub fn new(capacity: usize) -> Self {
        Self {
            subscriptions: Subscriptions {
                subscribers: Arc::new(Lock::new(Subscribers {
                    senders: Vec::new(),
                    open: true,
                })),
                capacity,
            },
        }
    }

    /// Adds a subscriber. It receives the messages sent from now on.
    pub fn subscribe(&self) -> Receiver<T> {
        self.subscriptions.subscribe()
    }

    /// A handle that adds subscribers from other tasks.
    pub fn subscriptions(&self) -> Subscriptions<T> {
        self.subscriptions.clone()
    }

    /// The number of subscribers that still hold their receiver.
    pub fn subscriber_count(&self) -> usize {
        self.subscriptions.current().len()
    }
}

impl<T: Clone> Topic<T> {
    /// Sends `value` to every subscriber and returns how many received it.
    ///
    /// Waits until every subscriber has room. If the future is dropped
    /// while it waits, no subscriber receives the value. With no
    /// subscribers the value is dropped and the result is zero.
    pub async fn send(&mut self, value: T) -> usize {
        let senders = self.subscriptions.current();
        for sender in &senders {
            sender.room().await;
        }
        deliver(&senders, value)
    }

    /// Sends like [`send`](Self::send), but waits only until `cancellation`
    /// is requested or `deadline` passes. Pass `None` for no deadline.
    ///
    /// On an error no subscriber has received the value, and the error
    /// returns it. A request already present wins over subscribers that
    /// have room, and cancellation wins over an equally ready deadline.
    #[cfg(feature = "tokio")]
    pub async fn send_until(
        &mut self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        value: T,
    ) -> Result<usize, SendError<T>> {
        let senders = self.subscriptions.current();
        let mut stop = pin!(shutdown(cancellation, deadline));
        // The first wait is for nothing, so a request already present is seen
        // even with no subscribers.
        let waits = std::iter::once(None).chain(senders.iter().map(Some));
        for sender in waits {
            let stopped = race((async { Some(stop.as_mut().await) }, async {
                if let Some(sender) = sender {
                    sender.room().await;
                }
                None
            }))
            .await;
            match stopped {
                Some(Shutdown::Cancelled) => return Err(SendError::Cancelled(value)),
                Some(Shutdown::DeadlineExceeded) => {
                    return Err(SendError::DeadlineExceeded(value));
                }
                None => {}
            }
        }
        Ok(deliver(&senders, value))
    }
}

/// Gives each subscriber a clone of `value`, and the last one `value` itself.
/// Every sender has room or is closed: the topic is the only sender, and
/// `&mut self` on its sends keeps another send from taking the room.
fn deliver<T: Clone>(senders: &[Sender<T>], value: T) -> usize {
    let mut delivered = 0;
    let mut value = Some(value);
    let mut senders = senders.iter().peekable();
    while let Some(sender) = senders.next() {
        let message = if senders.peek().is_some() {
            value.clone()
        } else {
            value.take()
        };
        let Some(message) = message else { break };
        match sender.try_send(message) {
            Ok(()) => delivered += 1,
            Err(TrySendError::Closed(_) | TrySendError::Full(_)) => {}
        }
    }
    delivered
}

impl<T> Subscriptions<T> {
    /// Adds a subscriber. It receives the messages sent from now on. If the
    /// topic has been dropped, the receiver is already closed.
    pub fn subscribe(&self) -> Receiver<T> {
        let (sender, receiver) = Channel::bounded(self.capacity).split();
        let mut subscribers = self.subscribers.lock();
        if subscribers.open {
            subscribers.senders.push(sender);
        }
        receiver
    }

    /// The senders of the subscribers that still hold their receiver.
    fn current(&self) -> Vec<Sender<T>> {
        let mut subscribers = self.subscribers.lock();
        subscribers.senders.retain(|sender| !sender.is_closed());
        subscribers.senders.clone()
    }
}

impl<T> Clone for Subscriptions<T> {
    fn clone(&self) -> Self {
        Self {
            subscribers: Arc::clone(&self.subscribers),
            capacity: self.capacity,
        }
    }
}

impl<T> Drop for Topic<T> {
    fn drop(&mut self) {
        let senders = {
            let mut subscribers = self.subscriptions.subscribers.lock();
            subscribers.open = false;
            std::mem::take(&mut subscribers.senders)
        };
        // Dropping the last sender of each queue closes it.
        drop(senders);
    }
}

impl<T> fmt::Debug for Topic<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Topic")
            .field("subscribers", &self.subscriber_count())
            .field("capacity", &self.subscriptions.capacity.max(1))
            .finish()
    }
}

impl<T> fmt::Debug for Subscriptions<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscriptions")
            .field("capacity", &self.capacity.max(1))
            .finish_non_exhaustive()
    }
}
