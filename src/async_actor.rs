use std::{fmt, future::Future, pin::Pin};

use tokio::time::Instant;

use crate::{
    Cancellation, Channel, Closed, ReceiveError, Reply, SendError, Str, Tasks, TrySendError, time,
};

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

async fn run_worker<M, S, E, H>(
    worker: MailboxGuard<M>,
    mut state: S,
    mut handler: H,
) -> Result<(), E>
where
    M: Send + 'static,
    S: Send + 'static,
    E: Send + 'static,
    H: for<'a> FnMut(&'a mut S, M) -> Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>
        + Send
        + 'static,
{
    while let Some(message) = worker.0.receive().await {
        handler(&mut state, message).await?;
    }
    Ok(())
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
        drop(tasks.spawn(run_worker(worker, state, handler)));
        Self { mailbox }
    }

    /// Starts a named worker in `tasks`, with the same contract as
    /// [`new`](Self::new).
    ///
    /// Delegates worker creation to [`Tasks::spawn_named`]. With the `tracing`
    /// feature, the worker runs inside an `adamantium.task` span with a
    /// `task.name` field, parented to the current span, and is reported at scope
    /// deadlines while unfinished. With the `deterministic` feature,
    /// `adamantium::testing::explain` labels the worker with `name` in its
    /// timeline. Without `tracing` (and outside timeline explanation), `name`
    /// is neither converted nor stored.
    pub fn new_named<S, E, H>(
        tasks: &mut Tasks<E>,
        name: impl Into<Str>,
        state: S,
        capacity: usize,
        handler: H,
    ) -> Self
    where
        S: Send + 'static,
        E: Send + 'static,
        H: for<'a> FnMut(&'a mut S, M) -> Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>
            + Send
            + 'static,
    {
        let mailbox = Channel::bounded(capacity);
        let worker = MailboxGuard(mailbox.clone());
        drop(tasks.spawn_named(name, run_worker(worker, state, handler)));
        Self { mailbox }
    }

    /// Enqueues a message, waiting while the mailbox is full.
    ///
    /// Success means accepted, not processed or durably committed. If closed,
    /// returns the original message in [`Closed`]. Dropping a pending send
    /// cancels that send and drops its message. For request-reply exchanges,
    /// see [`call`](Self::call) and [`call_until`](Self::call_until).
    pub async fn send(&self, message: M) -> Result<(), Closed<M>> {
        self.mailbox.send(message).await
    }

    /// Enqueues a message without waiting, returning the message in
    /// [`TrySendError::Full`] if the mailbox is full or
    /// [`TrySendError::Closed`] if it is closed.
    ///
    /// Matches [`Channel::try_send`].
    pub fn try_send(&self, message: M) -> Result<(), TrySendError<M>> {
        self.mailbox.try_send(message)
    }

    /// Enqueues a message, waiting while the mailbox is full, until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline uses [`crate::time`].
    ///
    /// Matches [`Channel::send_until`]: every failure hands the message back in
    /// [`SendError`], a shutdown request already present wins over room in the
    /// mailbox, and cancellation wins over an equally ready deadline. The
    /// message enters the mailbox only when the send succeeds.
    pub async fn send_until(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        message: M,
    ) -> Result<(), SendError<M>> {
        self.mailbox
            .send_until(cancellation, deadline, message)
            .await
    }

    /// Sends a request built with a one-shot [`Reply`] and waits for its answer.
    ///
    /// Returns `Some(answer)` when the handler replies, or `None` if the
    /// mailbox is closed before the message is accepted, or if the worker ends,
    /// fails, is cancelled, or drops the [`Reply`] without sending. Closing the
    /// mailbox with [`close`](Self::close) after the message is accepted still
    /// lets the worker drain and reply.
    ///
    /// Dropping this future while waiting for mailbox room discards the unsent
    /// message. Dropping it after the message was accepted drops the reply
    /// receiver, so the handler's [`Reply::is_closed`] becomes `true`,
    /// [`Reply::closed`] completes, and [`Reply::send`] hands the answer back
    /// in [`Closed`].
    ///
    /// ```
    /// use adamantium::{AsyncActor, Reply, Tasks};
    ///
    /// # #[tokio::main] async fn main() -> Result<(), &'static str> {
    /// struct Add(u64, Reply<u64>);
    ///
    /// let total = Tasks::run(async |tasks| {
    ///     let actor = AsyncActor::new(tasks, 0u64, 4, |sum, Add(n, reply)| {
    ///         Box::pin(async move {
    ///             *sum += n;
    ///             reply.send(*sum).ok();
    ///             Ok(())
    ///         })
    ///     });
    ///     let total = actor.call(|reply| Add(42, reply)).await.ok_or("closed")?;
    ///     actor.close();
    ///     Ok(total)
    /// })
    /// .await?;
    /// assert_eq!(total, 42);
    /// # Ok(()) }
    /// ```
    pub async fn call<R: Send>(&self, request: impl FnOnce(Reply<R>) -> M + Send) -> Option<R> {
        if self.is_closed() {
            return None;
        }
        let (reply, answer) = Reply::pair();
        self.send(request(reply)).await.ok()?;
        answer.receive().await
    }

    /// Sends a request like [`call`](Self::call), waiting only until
    /// `cancellation` is requested or the absolute `deadline` passes. Pass
    /// `None` for no deadline. A deadline uses [`crate::time`].
    ///
    /// One cancellation signal and deadline govern both waiting for mailbox
    /// room and waiting for the reply. As in [`send_until`](Self::send_until)
    /// and [`ReplyReceiver::receive_until`](crate::ReplyReceiver::receive_until),
    /// a shutdown request already present wins over mailbox room, closure, or a
    /// ready reply, and cancellation wins over an equally ready deadline.
    ///
    /// If shutdown wins before the message enters the mailbox, the message is
    /// not delivered and the handler never sees it. If shutdown wins after the
    /// message was accepted (or if this future is dropped), the internal
    /// [`ReplyReceiver`](crate::ReplyReceiver) is dropped before returning: the
    /// worker keeps running, and the handler's [`Reply`] immediately reports
    /// [`Reply::is_closed`] as `true`, completes [`Reply::closed`], and returns
    /// [`Closed`] from [`Reply::send`] so the handler can abandon unwanted work
    /// without failing the worker. If the handler already sent its reply before
    /// shutdown won, `Reply::send` returned `Ok(())` and the unreceived answer
    /// is dropped when `call_until` returns.
    pub async fn call_until<R: Send>(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        request: impl FnOnce(Reply<R>) -> M + Send,
    ) -> Result<R, ReceiveError> {
        let closed = self.is_closed();
        if cancellation.is_cancelled() {
            return Err(ReceiveError::Cancelled);
        }
        if deadline.is_some_and(|deadline| deadline <= time::now()) {
            return Err(ReceiveError::DeadlineExceeded);
        }
        if closed {
            return Err(ReceiveError::Closed);
        }
        let (reply, answer) = Reply::pair();
        match self
            .send_until(cancellation, deadline, request(reply))
            .await
        {
            Ok(()) => answer.receive_until(cancellation, deadline).await,
            Err(SendError::Closed(_)) => Err(ReceiveError::Closed),
            Err(SendError::Cancelled(_)) => Err(ReceiveError::Cancelled),
            Err(SendError::DeadlineExceeded(_)) => Err(ReceiveError::DeadlineExceeded),
        }
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
