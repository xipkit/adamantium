use std::{fmt, future::Future, pin::Pin};

use crate::{Channel, Closed, Tasks};

/// A send-only mailbox for state owned by one asynchronous worker.
///
/// The worker handles messages in enqueue order and can borrow its state
/// across awaits without a lock. Call [`Self::close`] to stop accepting sends
/// and drain the queue. Dropping every handle leaves the mailbox open.
///
/// A handler error fails the scope as an orphaned [`Task`](crate::Task) error.
/// Failure, panic, or scope cancellation closes the mailbox and discards the
/// queue, even before the first poll. Cancellation can interrupt in-flight
/// I/O. Await scope shutdown to wait for cleanup.
///
/// To avoid deadlock, a handler must never wait for its own worker to advance.
/// This includes sending to its own full mailbox or waiting on another worker
/// that needs this one.
///
/// ```
/// use adamantium::{AsyncActor, Tasks};
///
/// # #[tokio::main] async fn main() -> Result<(), &'static str> {
/// Tasks::run(async |tasks| {
///     let actor = AsyncActor::new(tasks, Vec::new(), 4, |state, message: u64| {
///         Box::pin(async move {
///             tokio::task::yield_now().await;
///             state.push(message);
///             Ok(())
///         })
///     });
///     actor.send(42).await.map_err(|_| "closed")?;
///     actor.close();
///     Ok(())
/// }).await
/// # }
/// ```
pub struct AsyncActor<M> {
    mailbox: Channel<M>,
}

// Create the guard before the first poll so early abort drains the queue.
// Close before draining to prevent new sends.
struct MailboxGuard<M>(Channel<M>);

impl<M> Drop for MailboxGuard<M> {
    fn drop(&mut self) {
        self.0.close();
        while self.0.try_receive().is_ok() {}
    }
}

impl<M: Send + 'static> AsyncActor<M> {
    /// Starts one worker in `tasks`, owning `state` and a bounded mailbox.
    ///
    /// `capacity` bounds queued messages and treats zero as one. One more
    /// message may be in the handler. The boxed `Send` future can borrow
    /// the state until it completes. Requires a Tokio runtime.
    ///
    /// A handler cannot keep a non-`Send` value across an await:
    ///
    /// ```compile_fail
    /// use adamantium::{AsyncActor, Tasks};
    /// let mut tasks = Tasks::<()>::new();
    /// let actor = AsyncActor::new(&mut tasks, (), 1, |_, (): ()| {
    ///     Box::pin(async move {
    ///         let local = std::rc::Rc::new(42);
    ///         tokio::task::yield_now().await;
    ///         drop(local);
    ///         Ok(())
    ///     })
    /// });
    /// ```
    pub fn new<S, E, H>(tasks: &mut Tasks<E>, state: S, capacity: usize, handler: H) -> Self
    where
        S: Send + 'static,
        E: Send + 'static,
        H: for<'a> FnMut(&'a mut S, M) -> Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>
            + Send
            + 'static,
    {
        let mailbox = Channel::bounded(capacity);
        let worker = MailboxGuard(mailbox.clone());
        drop(tasks.spawn(async move {
            let worker = worker;
            let mut state = state;
            let mut handler = handler;
            while let Some(message) = worker.0.receive().await {
                handler(&mut state, message).await?;
            }
            Ok(())
        }));
        Self { mailbox }
    }

    /// Enqueues a message, waiting while the mailbox is full.
    ///
    /// Success means accepted, not processed or durably committed. If closed,
    /// returns the original message in [`Closed`]. Dropping a pending send
    /// cancels that send and drops its message. Replies can be explicit fields
    /// in the message type; the mailbox itself provides no request/reply protocol.
    pub async fn send(&self, message: M) -> Result<(), Closed<M>> {
        self.mailbox.send(message).await
    }
}

impl<M> AsyncActor<M> {
    /// Rejects new sends and wakes blocked senders. The worker finishes its
    /// current handler and drains queued messages unless it fails or is cancelled.
    /// Returns `false` if already closed. Closing any clone closes every clone.
    pub fn close(&self) -> bool {
        self.mailbox.close()
    }

    /// Whether the mailbox is closed; accepted work may still be running.
    pub fn is_closed(&self) -> bool {
        self.mailbox.is_closed()
    }
}

impl<M> Clone for AsyncActor<M> {
    fn clone(&self) -> Self {
        Self {
            mailbox: self.mailbox.clone(),
        }
    }
}

impl<M> fmt::Debug for AsyncActor<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncActor")
            .field("mailbox", &self.mailbox)
            .finish()
    }
}
