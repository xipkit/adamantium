use std::{fmt, future::Future, pin::Pin};

use crate::{Channel, Closed, Tasks};

/// A send-only mailbox for state owned by one asynchronous worker.
///
/// Unlike [`crate::Actor`], the worker can borrow its state across an await.
/// It finishes each handler before receiving the next message, in enqueue
/// order. Concurrent senders have no ordering guarantee beyond that order.
/// No shared-state lock is held while handling a message.
///
/// Call [`close`](Self::close) before waiting for successful scope completion:
/// it rejects new sends and lets the worker drain accepted messages. Dropping
/// all mailbox handles does **not** close the worker's channel. A handler error
/// stops the worker and fails the scope. Nobody holds the worker's [`Task`](crate::Task)
/// handle, so the error is [orphaned](crate::Task): under [`Tasks::run`] or
/// [`Tasks::run_until`] it also stops a still-running scope body at its next
/// `await`.
///
/// Failure, panic, or scope cancellation closes the mailbox and discards queued
/// messages, even if handles survive or the worker has never been polled.
/// Cancellation may interrupt in-flight I/O: accepted messages are not guaranteed
/// delivery, and external effects cannot be rolled back. Dropping a scope requests
/// cancellation without waiting; await its shutdown to wait for worker cleanup.
///
/// Mailbox cycles can deadlock. A handler must not await work that requires its
/// own worker to advance, including sending to its own full mailbox or awaiting
/// a reply from another worker that is waiting on this one.
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

// Constructed outside the future so aborting before its first poll also cleans
// up the queue. Closing before draining prevents sends from refilling it.
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
    /// `capacity` bounds queued messages, with one additional message being
    /// handled; it does not bound producer tasks waiting to send. Zero is treated
    /// as one. `handler` returns a boxed `Send` future that can borrow the state
    /// until it completes. Use `|state, message| Box::pin(async move { ... })`.
    /// Boxing makes that borrowing future's `Send` bound expressible on stable
    /// Rust. The state and handler need `Send`, but do not need `Sync` or `Clone`.
    ///
    /// Requires a Tokio runtime, just like [`Tasks::spawn`].
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
