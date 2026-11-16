use std::{
    any::Any, collections::BTreeMap, fmt, future::Future, panic, pin::pin, sync::Arc, task::Poll,
};

use crate::runtime::{
    Guard, Lock, World,
    task::{JoinError, JoinSet},
    wait::WaitQueue,
};
use tokio::time::Instant;

use crate::{Closed, Reply, ReplyReceiver, Watch, time};

mod stream;
#[cfg(feature = "tracing")]
mod trace;
pub use stream::{
    concurrently_ordered, concurrently_ordered_until, concurrently_unordered,
    concurrently_unordered_until,
};

/// A cloneable, one-way shutdown request shared by a caller and its work.
///
/// Cancelling any clone wakes all [`cancelled`](Self::cancelled) waiters.
/// Dropping a clone does not cancel. Use a fresh signal for each scope: a
/// cancelled signal cannot be reset, and scopes also cancel it on shutdown.
pub struct Cancellation {
    world: World,
    requested: Watch<bool>,
}

impl Cancellation {
    /// Creates a signal with no shutdown requested.
    pub fn new() -> Self {
        Self {
            world: World::current(),
            requested: Watch::new(false),
        }
    }

    /// Requests shutdown. Repeated requests have the same effect as one.
    pub fn cancel(&self) {
        self.world.check();
        self.request();
    }

    /// Records the request. A scope's destructor calls this directly, since
    /// it may run while unwinding, when `cancel`'s harness check would panic
    /// a second time.
    fn request(&self) {
        // This handle keeps the watch open, so the send cannot fail.
        self.requested.send(true).unwrap_or(());
    }

    /// Whether shutdown has been requested, including from a blocking thread.
    pub fn is_cancelled(&self) -> bool {
        self.world.check();
        self.requested.get()
    }

    /// Waits for shutdown, returning immediately if already requested.
    /// Dropping this future does not lose the request.
    pub async fn cancelled(&self) {
        crate::runtime::in_world(self.world, async move {
            let mut requested = self.requested.subscribe();
            // Borrowing self keeps a handle alive, so the watch cannot close.
            requested.wait_for(|requested| *requested).await;
        })
        .await
    }

    /// Requests shutdown on SIGINT or SIGTERM on Unix, or Ctrl-C on Windows.
    ///
    /// Requires the opt-in `signal` feature and a Tokio runtime with signal
    /// handling enabled. Listening starts when this future is first polled;
    /// registration errors are returned without requesting cancellation.
    /// It also returns successfully if this signal is cancelled another way.
    /// An already cancelled signal does not register process handlers.
    ///
    /// Await this alongside the work that observes this cancellation, for
    /// example with `tokio::try_join!`. No background task is spawned. Dropping
    /// the future stops listening without cancelling, but Tokio's process-wide
    /// signal handlers remain installed, replacing the default termination
    /// behavior even after the listener is dropped.
    #[cfg(all(feature = "signal", any(unix, windows)))]
    pub async fn cancel_on_signal(&self) -> std::io::Result<()> {
        crate::runtime::production_only("OS signal handling");
        #[cfg(unix)]
        let received = async {
            use tokio::signal::unix::{SignalKind, signal};

            let mut interrupt = signal(SignalKind::interrupt())?;
            let mut terminate = signal(SignalKind::terminate())?;
            tokio::select! {
                biased;
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
            Ok::<(), std::io::Error>(())
        };
        #[cfg(windows)]
        let received = tokio::signal::ctrl_c();

        tokio::select! {
            biased;
            () = self.cancelled() => Ok(()),
            result = received => {
                result?;
                self.cancel();
                Ok(())
            }
        }
    }
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.world.check();
        f.debug_struct("Cancellation")
            .field("requested", &self.requested.get())
            .finish()
    }
}

#[cfg(feature = "deterministic")]
impl Drop for Cancellation {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.world.check();
        }
    }
}

impl Clone for Cancellation {
    fn clone(&self) -> Self {
        self.world.check();
        Self {
            world: self.world,
            requested: self.requested.clone(),
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a cancellable scope did not produce a value.
#[derive(Debug, PartialEq, Eq)]
pub enum ScopeError<E> {
    /// The body or a child returned an error.
    Failed(E),
    /// The caller requested shutdown.
    Cancelled,
    /// The scope's deadline elapsed.
    DeadlineExceeded,
}

impl<E: fmt::Display> fmt::Display for ScopeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(error) => write!(f, "scope failed: {error}"),
            Self::Cancelled => f.write_str("scope cancelled"),
            Self::DeadlineExceeded => f.write_str("scope deadline exceeded"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ScopeError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// Why a wait ended before its work did: the caller requested shutdown, or
/// the deadline elapsed.
pub(crate) enum Shutdown {
    Cancelled,
    DeadlineExceeded,
}

impl Shutdown {
    pub(crate) fn scope_error<E>(self) -> ScopeError<E> {
        match self {
            Self::Cancelled => ScopeError::Cancelled,
            Self::DeadlineExceeded => ScopeError::DeadlineExceeded,
        }
    }
}

/// Waits for cancellation or the deadline, whichever comes first. A request
/// already present at a poll is reported by that poll, and cancellation wins
/// over an equally ready deadline.
pub(crate) async fn shutdown(cancellation: &Cancellation, deadline: Option<Instant>) -> Shutdown {
    let mut cancelled = pin!(cancellation.cancelled());
    let mut elapsed = pin!(async {
        match deadline {
            Some(deadline) => time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    });
    std::future::poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Shutdown::Cancelled);
        }
        // Tokio timers round up to milliseconds. Check the clock as well so an
        // expired deadline wins before polling the waiter's work on every poll.
        if deadline.is_some_and(|deadline| deadline <= time::now())
            || elapsed.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Shutdown::DeadlineExceeded);
        }
        Poll::Pending
    })
    .await
}

/// The errors of failed tasks that nobody has claimed, oldest first. A task
/// records its error here as it fails, whatever became of its handle, so the
/// scope can always find it. A panic already observed by the scope also stays
/// here until draining finishes, even if cancellation interrupts the drain.
struct Errors<E> {
    next: u64,
    recorded: BTreeMap<u64, E>,
    panic: Option<Box<dyn Any + Send>>,
    /// Whether a recorded error has lost its handle, so that nothing but the
    /// scope can ever report it.
    orphaned: bool,
}

/// What a scope shares with its tasks and their handles.
struct Ledger<E> {
    errors: Lock<Errors<E>>,
    /// Woken when an error is first orphaned.
    on_orphan: WaitQueue,
}

impl<E> Ledger<E> {
    fn lock(&self) -> Guard<'_, Errors<E>> {
        self.errors.lock()
    }
}

/// What a failed task sends its handle: where to claim the error.
struct Receipt(u64);

type Shared<V> = Arc<Lock<V>>;

type SharedErrors<E> = Arc<Ledger<E>>;

fn shared<V>(value: V) -> Shared<V> {
    Arc::new(Lock::new(value))
}

fn lock<V>(shared: &Shared<V>) -> Guard<'_, V> {
    shared.lock()
}

fn record<E>(errors: &SharedErrors<E>, error: E) -> Receipt {
    let mut errors = errors.lock();
    let place = errors.next;
    errors.next += 1;
    errors.recorded.insert(place, error);
    Receipt(place)
}

fn claim<E>(errors: &SharedErrors<E>, receipt: Receipt) -> Option<E> {
    errors.lock().recorded.remove(&receipt.0)
}

fn any_recorded<E>(errors: &SharedErrors<E>) -> bool {
    !errors.lock().recorded.is_empty()
}

fn take_oldest<E>(errors: &SharedErrors<E>) -> Option<E> {
    errors.lock().recorded.pop_first().map(|(_, error)| error)
}

/// Marks that a recorded error can no longer reach anyone through its handle,
/// and wakes a body waiting in [`orphaned`].
fn orphan<E>(errors: &SharedErrors<E>) {
    let first = !std::mem::replace(&mut errors.lock().orphaned, true);
    if first {
        errors.on_orphan.notify_all();
    }
}

/// Waits until some error is orphaned, returning at once if one already is.
async fn orphaned<E>(errors: &SharedErrors<E>) {
    loop {
        if errors.lock().orphaned {
            return;
        }
        // Listen before checking again, so an orphan between the two checks
        // still wakes this wait.
        let listener = errors.on_orphan.listen();
        if errors.lock().orphaned {
            return;
        }
        listener.await;
    }
}

/// Sends a task's result to its handle. An error the handle is no longer
/// there to take stays recorded, now orphaned.
fn deliver<T, E>(
    errors: &SharedErrors<E>,
    tx: Reply<Result<T, Receipt>>,
    result: Result<T, Receipt>,
) {
    if let Err(Closed(Err(_))) = tx.send(result) {
        orphan(errors);
    }
}

fn no_result() -> ! {
    panic::resume_unwind(Box::new(
        "adamantium: a task ended without a result and without a panic",
    ))
}

fn no_orphan() -> ! {
    panic::resume_unwind(Box::new(
        "adamantium: an orphaned error was gone when the scope ended",
    ))
}

/// A scope that owns the tasks spawned inside it.
///
/// [`join`](Self::join) waits for every registered task,
/// and an error in any of them ends the scope with that error after the
/// others are aborted. A task's error reaches the body through
/// [`value`](Self::value); an error nobody asked for is reported when the
/// scope joins, so it is never lost. Under [`run`](Self::run) or
/// [`run_until`](Self::run_until), an error that nobody can ask for any more
/// also ends the body at once: see [`Task`]. A panic in a task is resumed in the
/// scope, as a thread panic is resumed by a scoped thread. Dropping a scope
/// without joining aborts its async tasks without waiting for them. Started
/// blocking closures continue running. See [`run_until`](Self::run_until)
/// for cancellation, deadlines, and the cleanup contract.
///
/// Spawning needs a Tokio runtime, as every spawn does. Use
/// [`run`](Self::run) for the common shape, or [`new`](Self::new),
/// [`spawn`](Self::spawn) and [`join`](Self::join) by hand.
pub struct Tasks<E> {
    set: JoinSet,
    world: World,
    errors: SharedErrors<E>,
    cancellation: Option<Cancellation>,
    #[cfg(feature = "tracing")]
    named: Option<Shared<trace::Running>>,
}

/// The handle to a task spawned in a [`Tasks`] scope.
///
/// Pass it to [`Tasks::value`] on the scope that spawned the task to wait for
/// its result. Dropping it or keeping it is fine: the task still runs, and an
/// error nobody took with `value` is reported when the scope joins.
///
/// Once a task has failed and its handle is gone, whichever came first, the
/// error is orphaned: nobody can take it any more. In
/// [`Tasks::run`] and [`Tasks::run_until`] an orphaned error stops the body at
/// its next `await`, as a failed child stops a Trio nursery: the body is
/// dropped, the other tasks are aborted and drained, and the scope returns
/// the oldest unclaimed error. A handle that is kept, even one never awaited,
/// can still take its error, so that error waits until the handle is dropped
/// or passed to `value`. A scope used by hand, with [`Tasks::join`] or
/// [`Tasks::join_until`], has no body to stop and reports the error when it
/// joins.
pub struct Task<T, E> {
    world: World,
    rx: ReplyReceiver<Result<T, Receipt>>,
    errors: SharedErrors<E>,
}

impl<E: Send + 'static> Tasks<E> {
    /// An empty scope.
    pub fn new() -> Self {
        Self {
            set: JoinSet::new(),
            world: World::current(),
            errors: Arc::new(Ledger {
                errors: Lock::new(Errors {
                    next: 0,
                    recorded: BTreeMap::new(),
                    panic: None,
                    orphaned: false,
                }),
                on_orphan: WaitQueue::new(),
            }),
            cancellation: None,
            #[cfg(feature = "tracing")]
            named: None,
        }
    }

    /// Runs `body` with a scope, then joins the scope. This is the usual
    /// way to use `Tasks`. An [orphaned](Task) task error stops the body at its
    /// next `await` and becomes the scope's error.
    ///
    /// ```
    /// # use adamantium::Tasks;
    /// # async fn fetch(url: &str) -> Result<String, String> { Ok(url.to_owned()) }
    /// # #[tokio::main] async fn main() -> Result<(), String> {
    /// let (profile, feed) = Tasks::run(async |t| {
    ///     let profile = t.spawn(fetch("https://example.com/profile"));
    ///     let feed = t.spawn(fetch("https://example.com/feed"));
    ///     Ok((t.value(profile).await?, t.value(feed).await?))
    /// })
    /// .await?;
    /// # assert_eq!(profile, "https://example.com/profile");
    /// # assert_eq!(feed, "https://example.com/feed");
    /// # Ok(()) }
    /// ```
    #[allow(
        clippy::future_not_send,
        reason = "Send follows from the closure's future, which stable Rust cannot name in a bound; callers' futures are still checked"
    )]
    pub async fn run<R, F>(body: F) -> Result<R, E>
    where
        R: Send,
        F: AsyncFnOnce(&mut Tasks<E>) -> Result<R, E>,
    {
        crate::runtime::in_world(World::current(), async move {
            let mut tasks = Self::new();
            let errors = Arc::clone(&tasks.errors);
            tokio::select! {
                biased;
                () = orphaned(&errors) => {},
                result = body(&mut tasks) => return tasks.join(result).await,
            }
            tasks.fail().await
        })
        .await
    }

    /// Runs a scope until completion, cancellation, or an absolute deadline.
    /// Pass `None` for no deadline. A deadline uses [`crate::time`].
    /// The body and children can observe clones of `cancellation` themselves.
    ///
    /// A request already present at a poll wins over polling the body, including
    /// before its first poll. Cancellation wins over an equally ready deadline.
    /// On shutdown the body is dropped, the signal is cancelled, and async
    /// children are aborted and drained. Their synchronous destructors finish
    /// before return; async cleanup is not guaranteed a chance to run. Registered
    /// [`spawn_blocking`](Self::spawn_blocking) closures that have started are
    /// waited for, so a deadline bounds work, not cleanup time. Blocking work
    /// should check the signal and finish promptly.
    ///
    /// Precedence is: child panic (resumed after draining), an already returned
    /// body error, oldest unclaimed child error (even during cleanup), then the
    /// selected shutdown reason. Once cleanup starts its reason is fixed.
    /// As in [`run`](Self::run), an [orphaned](Task) child error stops the body
    /// at its next `await`, which also cancels the signal, and the scope fails
    /// with the oldest unclaimed error. A shutdown request present at the same
    /// poll still wins the poll, but that error then wins the result.
    ///
    /// Dropping this future after it starts, or panicking in its body, signals
    /// cancellation and requests aborts, but cannot wait for cleanup. Started
    /// blocking closures may outlive it. To guarantee cleanup, request
    /// cancellation and keep awaiting this future instead of dropping it.
    ///
    /// ```
    /// use adamantium::{Cancellation, ScopeError, Tasks, time};
    /// use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let cancellation = Cancellation::new();
    /// let worker = cancellation.clone();
    /// let result = Tasks::<std::io::Error>::run_until(
    ///     cancellation,
    ///     Some(time::now() + Duration::from_secs(1)),
    ///     async |tasks| {
    ///         let work = tasks.spawn_blocking(move || {
    ///             // Real blocking work checks this between bounded operations.
    ///             if worker.is_cancelled() { return Ok(None); }
    ///             Ok(Some(42))
    ///         });
    ///         tasks.value(work).await
    ///     },
    /// ).await;
    /// assert!(matches!(result, Ok(Some(42)) | Err(ScopeError::DeadlineExceeded)));
    /// # }
    /// ```
    #[allow(clippy::future_not_send, reason = "see run")]
    pub async fn run_until<R, F>(
        cancellation: Cancellation,
        deadline: Option<Instant>,
        body: F,
    ) -> Result<R, ScopeError<E>>
    where
        R: Send,
        F: AsyncFnOnce(&mut Tasks<E>) -> Result<R, E>,
    {
        crate::runtime::in_world(World::current(), async move {
            let mut tasks = Self::new();
            tasks.cancellation = Some(cancellation.clone());
            let errors = Arc::clone(&tasks.errors);
            let reason = tokio::select! {
                biased;
                reason = shutdown(&cancellation, deadline) => reason,
                () = orphaned(&errors) => return tasks.fail().await.map_err(ScopeError::Failed),
                result = body(&mut tasks) => return tasks.join_until(cancellation, deadline, result).await,
            };
            #[cfg(feature = "tracing")]
            tasks.report_deadline(&reason);
            tasks.abort();
            tasks.join(Ok(())).await.map_err(ScopeError::Failed)?;
            Err(reason.scope_error())
        })
        .await
    }

    /// Starts `future` as a task of this scope and returns its handle.
    pub fn spawn<T, F>(&mut self, future: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let errors = Arc::clone(&self.errors);
        self.set.spawn(async move {
            let result = future.await.map_err(|error| record(&errors, error));
            // Without a handle to take it, a value is dropped.
            deliver(&errors, tx, result);
        });
        Task {
            world: self.world,
            rx,
            errors: Arc::clone(&self.errors),
        }
    }

    /// Starts a named task. With the `tracing` feature, its future is polled
    /// inside an `adamantium.task` span with a `task.name` field, parented to
    /// the current span. Unfinished named tasks are reported at scope deadlines.
    /// Without `tracing`, this behaves as [`spawn`](Self::spawn) without
    /// converting or storing the name.
    pub fn spawn_named<T, F>(&mut self, name: impl Into<crate::Str>, future: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.world.check();
        #[cfg(feature = "tracing")]
        {
            use tracing::{Instrument, instrument::WithSubscriber};

            let (running, span) = self.track(name.into());
            self.spawn(
                async move {
                    let _running = running;
                    future.await
                }
                .instrument(span)
                .with_current_subscriber(),
            )
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = name;
            self.spawn(future)
        }
    }

    /// Registers blocking work directly with this scope.
    ///
    /// Joining always waits for started closures, even after an error,
    /// cancellation, or deadline. Queued closures may be aborted before starting.
    /// Capture the signal passed to [`run_until`](Self::run_until) or
    /// [`join_until`](Self::join_until) to let a running closure cooperate.
    /// Dropping a cancellable scope signals shutdown but cannot wait for it.
    pub fn spawn_blocking<T, F>(&mut self, f: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let errors = Arc::clone(&self.errors);
        self.set.spawn_blocking(move || {
            let result = f().map_err(|error| record(&errors, error));
            // A dropped handle discards the value.
            deliver(&errors, tx, result);
        });
        Task {
            world: self.world,
            rx,
            errors: Arc::clone(&self.errors),
        }
    }

    /// Registers named blocking work, with the same cleanup contract as
    /// [`spawn_blocking`](Self::spawn_blocking). With `tracing`, the closure
    /// runs inside an `adamantium.task` span and is included in deadline reports
    /// until it finishes. Without `tracing`, the name is not converted or stored.
    pub fn spawn_blocking_named<T, F>(&mut self, name: impl Into<crate::Str>, f: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        #[cfg(feature = "tracing")]
        {
            let (running, span) = self.track(name.into());
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            self.spawn_blocking(move || {
                tracing::dispatcher::with_default(&dispatch, move || {
                    span.in_scope(move || {
                        let _running = running;
                        f()
                    })
                })
            })
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = name;
            self.spawn_blocking(f)
        }
    }

    /// Waits for a task and returns its result. If the task panicked, the
    /// panic is resumed here.
    pub async fn value<T: Send>(&mut self, task: Task<T, E>) -> Result<T, E> {
        crate::runtime::in_world(self.world, async move {
            let error = match task.rx.receive().await {
                Some(Ok(value)) => return Ok(value),
                Some(Err(receipt)) => claim(&task.errors, receipt),
                None => None,
            };
            match error {
                Some(error) => Err(error),
                // A task that ends without a result has panicked, so its join
                // result is a panic. Find it. A result whose error is gone came
                // from another scope, whose join has already reported it.
                None => self.resume_first_panic().await,
            }
        })
        .await
    }

    /// Waits for every task, then returns `body`'s result unless a task
    /// failed. Tasks are aborted as soon as an error is known, whether
    /// from `body` or from a task. The first error from a task, unless
    /// `body` already took it with [`value`](Self::value), is returned over
    /// a successful body; a failed body's error is returned over a task's.
    pub async fn join<R: Send>(mut self, body: Result<R, E>) -> Result<R, E> {
        crate::runtime::in_world(self.world, async move {
            self.settle(body.is_err()).await;
            self.result(body)
        })
        .await
    }

    /// Joins with cancellation and a deadline, using [`run_until`](Self::run_until)'s
    /// precedence and cleanup contract. The signal is registered immediately,
    /// even if the returned future is dropped before its first poll. Work spawned
    /// earlier may already be running when this method is called.
    pub fn join_until<R: Send>(
        mut self,
        cancellation: Cancellation,
        deadline: Option<Instant>,
        body: Result<R, E>,
    ) -> impl Future<Output = Result<R, ScopeError<E>>> + Send {
        self.world.check();
        self.cancellation = Some(cancellation.clone());
        crate::runtime::in_world(World::current(), async move {
            let reason = tokio::select! {
                biased;
                reason = shutdown(&cancellation, deadline) => Some(reason),
                () = self.settle(body.is_err()) => None,
            };
            if let Some(reason) = &reason {
                #[cfg(feature = "tracing")]
                self.report_deadline(reason);
                #[cfg(not(feature = "tracing"))]
                let _ = reason;
                self.abort();
                self.drain().await;
            }
            let value = self.result(body).map_err(ScopeError::Failed)?;
            match reason {
                Some(reason) => Err(reason.scope_error()),
                None => Ok(value),
            }
        })
    }

    async fn settle(&mut self, body_failed: bool) {
        let mut aborted = false;
        loop {
            // A task records its error just before it ends, so the end of
            // any task is the moment to look for a new one.
            if !aborted && (body_failed || any_recorded(&self.errors)) {
                self.abort();
                aborted = true;
            }
            match self.set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => self.remember_panic(join_error),
                None => break,
            }
        }
        self.propagate_panic();
    }

    /// Ends a scope whose body an orphaned error stopped.
    async fn fail<R>(mut self) -> Result<R, E> {
        self.settle(true).await;
        match take_oldest(&self.errors) {
            Some(error) => Err(error),
            // Only a handle can claim an error, and an orphaned one has none.
            None => no_orphan(),
        }
    }

    fn result<R>(&self, body: Result<R, E>) -> Result<R, E> {
        match (body, take_oldest(&self.errors)) {
            (Err(error), _) => Err(error),
            (Ok(_), Some(error)) => Err(error),
            (Ok(value), None) => Ok(value),
        }
    }

    /// The number of tasks that have not been joined.
    pub fn len(&self) -> usize {
        self.world.check();
        self.set.len()
    }

    /// Whether every spawned task has been joined.
    pub fn is_empty(&self) -> bool {
        self.world.check();
        self.set.is_empty()
    }

    async fn resume_first_panic<T>(&mut self) -> T {
        while let Some(next) = self.set.join_next().await {
            if let Err(join_error) = next {
                self.remember_panic(join_error);
                let panicked = self.errors.lock().panic.is_some();
                if panicked {
                    self.drain().await;
                }
            }
        }
        no_result()
    }

    // Keep the payload in the scope: cancellation can interrupt value or settle
    // while they drain, and must not discard an already observed panic.
    fn remember_panic(&mut self, join_error: JoinError) {
        if let Ok(payload) = join_error.try_into_panic() {
            {
                let mut errors = self.errors.lock();
                if errors.panic.is_none() {
                    errors.panic = Some(payload);
                }
            }
            self.abort();
        }
    }

    fn propagate_panic(&mut self) {
        let payload = self.errors.lock().panic.take();
        if let Some(payload) = payload {
            panic::resume_unwind(payload);
        }
    }

    fn abort(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        self.set.abort_all();
    }

    async fn drain(&mut self) {
        while let Some(next) = self.set.join_next().await {
            if let Err(error) = next {
                self.remember_panic(error);
            }
        }
        self.propagate_panic();
    }
}

impl<E> Drop for Tasks<E> {
    fn drop(&mut self) {
        #[cfg(feature = "deterministic")]
        if !std::thread::panicking() {
            self.world.check();
        }
        if std::thread::panicking() && self.world.is_model() && self.world != World::current() {
            return;
        }
        if let Some(cancellation) = &self.cancellation {
            cancellation.request();
        }
        // JoinSet's destructor requests aborts, but cannot await their completion.
    }
}

impl<E: Send + 'static> Default for Tasks<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> fmt::Debug for Tasks<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.world.check();
        f.debug_struct("Tasks")
            .field("len", &self.set.len())
            .finish()
    }
}

impl<T, E> Drop for Task<T, E> {
    fn drop(&mut self) {
        if std::thread::panicking() && self.world != World::current() {
            return;
        }
        // Closing first means a task failing from now on finds no handle and
        // orphans its error itself, while one that already failed left its
        // receipt here.
        self.rx.close();
        if let Ok(Err(_)) = self.rx.try_receive() {
            orphan(&self.errors);
        }
    }
}

impl<T, E> fmt::Debug for Task<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.world.check();
        f.write_str("Task")
    }
}

/// Runs `f` on every item, at most `limit` at a time, and returns the
/// results in input order, whatever order the items finish in. A `limit` of
/// zero means one. The first error to happen ends the whole operation, even
/// with earlier items still pending: no further item is taken, the running
/// items are aborted and drained, then the error is returned. A panic in an
/// item's future does the same and is resumed here.
///
/// Apart from calling `into_iter`, nothing happens until the returned future
/// is first polled, and a future that is never polled never calls `f`. Each
/// poll takes an item and calls `f` only while fewer than `limit` items are
/// unfinished, so at most `limit` of `f`'s futures exist at once however long
/// the input is. Finished results are held until the end, so they still take
/// memory in proportion to the items completed.
///
/// The iterator and `f` live in the returned future, which is why they must
/// be `Send`; `I` and the items need not be. Both run inside the future's
/// poll rather than in a task, so a slow iterator or `f` holds up whoever is
/// awaiting, including noticing that an item failed: put slow work in the
/// future `f` returns. A panic in the iterator or in `f` itself unwinds
/// through the poll and aborts the running items without waiting for them.
///
/// Each item runs as a task in its own scope, so polling needs a Tokio
/// runtime, and panics at the first item without one.
pub fn concurrently<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    mut f: F,
) -> impl Future<Output = Result<Vec<T>, E>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    // The loop comes back for another item after every join, and an iterator
    // may resume or panic if it is called again after its first `None`.
    let mut items = items.into_iter().fuse();
    let limit = limit.max(1);
    crate::runtime::in_world(World::current(), async move {
        let mut tasks = Tasks::new();
        // Tasks report here rather than through handles, so items that finish
        // behind a slow one leave only their results behind.
        let finished = shared(Vec::new());
        let mut started = 0;
        // Fill the window, then wait for a place to open. A task records its
        // error just before it ends, so this stops before taking another item.
        while !any_recorded(&tasks.errors) {
            if tasks.len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let finished = Arc::clone(&finished);
                tasks.spawn(async move {
                    let value = future.await?;
                    lock(&finished).push((index, value));
                    Ok(())
                });
                continue;
            }
            match tasks.set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => {
                    // Nothing here has aborted a task, so a cancellation means
                    // its runtime shut down, and that item will have no result.
                    tasks.remember_panic(join_error);
                    tasks.abort();
                    break;
                }
                None => break,
            }
        }
        tasks.join(Ok(())).await?;
        let mut finished = std::mem::take(&mut *lock(&finished));
        if finished.len() != started {
            no_result();
        }
        finished.sort_unstable_by_key(|(index, _)| *index);
        let mut values: Vec<T> = finished.into_iter().map(|(_, value)| value).collect();
        // The indexes and the growth of `finished` left spare room that the
        // caller would otherwise keep for as long as it keeps the results.
        values.shrink_to_fit();
        Ok(values)
    })
}

/// What the items of a [`first_ok`] have produced so far.
struct Outcomes<T, E> {
    winner: Option<T>,
    errors: Vec<(usize, E)>,
}

/// Runs `f` on items, at most `limit` at a time, and returns the first
/// success to happen. A `limit` of zero means one. With a limit of one this
/// tries the items in turn until one succeeds; with a limit as long as the
/// input it races them all.
///
/// A failure only frees a place for the next item. The first success ends
/// the whole operation: no further item is taken, the running items are
/// aborted and drained, then the value is returned. A success that lands
/// while they drain is dropped. If every item fails, all of their errors
/// come back in input order, so the error at index `i` is item `i`'s; an
/// empty input fails with no errors. A panic in an item's future aborts and
/// drains the others and is resumed here, even after a success.
///
/// Laziness, the `Send` bounds, where the iterator and `f` run, and the need
/// for a Tokio runtime are as for [`concurrently`].
///
/// ```
/// use adamantium::first_ok;
/// # async fn fetch(mirror: &str) -> Result<String, String> {
/// #     if mirror == "a.example.com" { Err(format!("{mirror} is down")) } else { Ok(mirror.to_owned()) }
/// # }
/// # #[tokio::main] async fn main() -> Result<(), Vec<String>> {
/// let mirrors = ["a.example.com", "b.example.com", "c.example.com"];
/// let page = first_ok(mirrors, 1, fetch).await?;
/// assert_eq!(page, "b.example.com");
/// # Ok(()) }
/// ```
pub fn first_ok<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    mut f: F,
) -> impl Future<Output = Result<T, Vec<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let mut items = items.into_iter().fuse();
    let limit = limit.max(1);
    crate::runtime::in_world(World::current(), async move {
        // Items report here rather than as task errors, which would make the
        // scope abort the others at the first failure.
        let mut tasks = Tasks::<std::convert::Infallible>::new();
        let outcomes = shared(Outcomes {
            winner: None,
            errors: Vec::new(),
        });
        let mut started = 0;
        // A task reports just before it ends, so this stops before taking
        // another item once one has succeeded.
        while lock(&outcomes).winner.is_none() {
            if tasks.len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let outcomes = Arc::clone(&outcomes);
                tasks.spawn(async move {
                    let result = future.await;
                    let mut outcomes = lock(&outcomes);
                    match result {
                        Ok(value) => {
                            if outcomes.winner.is_none() {
                                outcomes.winner = Some(value);
                            }
                        }
                        Err(error) => outcomes.errors.push((index, error)),
                    }
                    Ok(())
                });
                continue;
            }
            match tasks.set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => {
                    // Nothing here has aborted a task yet, so a cancellation
                    // means its runtime shut down, and that item has no result.
                    tasks.remember_panic(join_error);
                    break;
                }
                None => break,
            }
        }
        tasks.abort();
        let Ok(()) = tasks.join(Ok(())).await;
        let (winner, mut errors) = {
            let mut outcomes = lock(&outcomes);
            (outcomes.winner.take(), std::mem::take(&mut outcomes.errors))
        };
        if let Some(value) = winner {
            return Ok(value);
        }
        if errors.len() != started {
            no_result();
        }
        errors.sort_unstable_by_key(|(index, _)| *index);
        Err(errors.into_iter().map(|(_, error)| error).collect())
    })
}

/// Runs `f` on a thread meant for blocking work and returns its result. Use
/// it for code that blocks the thread it runs on, such as synchronous file
/// or network calls. A panic in `f` is resumed here.
///
/// WASI has one thread, so there `f` runs on it and other tasks wait until
/// `f` returns.
///
/// Dropping this future detaches a started closure: it cannot be forcibly
/// aborted, even inside an async task owned by a scope. Use
/// [`Tasks::spawn_blocking`] when scope shutdown must wait for blocking work.
pub async fn blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    crate::runtime::in_world(World::current(), async move {
        let handle = crate::runtime::task::spawn_blocking(f);
        match handle.await {
            Ok(value) => value,
            Err(join_error) => {
                panic::resume_unwind(join_error.try_into_panic().unwrap_or_else(|e| Box::new(e)))
            }
        }
    })
    .await
}

#[cfg(all(test, feature = "deterministic"))]
mod schedule_tests {
    use super::*;
    use crate::testing::{self, Config};

    #[test]
    fn oldest_unclaimed_error_and_body_precedence_survive_replay() {
        async fn scenario() {
            let mut tasks = Tasks::<&str>::new();
            let first = tasks.spawn(async { Err::<(), _>("first") });
            while tasks.errors.lock().recorded.len() != 1 {
                shuttle::future::yield_now().await;
            }
            let second = tasks.spawn(async { Err::<(), _>("second") });
            while tasks.errors.lock().recorded.len() != 2 {
                shuttle::future::yield_now().await;
            }
            assert_eq!(tasks.value(first).await, Err("first"));
            let third = tasks.spawn(async { Err::<(), _>("third") });
            while tasks.errors.lock().recorded.len() != 2 {
                shuttle::future::yield_now().await;
            }
            // Retained result handles cannot hide errors from join.
            assert_eq!(tasks.join(Ok(())).await, Err("second"));
            drop((second, third));

            let mut tasks = Tasks::<&str>::new();
            let child = tasks.spawn(async { Err::<(), _>("child") });
            while !any_recorded(&tasks.errors) {
                shuttle::future::yield_now().await;
            }
            assert_eq!(tasks.join(Err::<(), _>("body")).await, Err("body"));
            drop(child);
        }
        let config = Config {
            seed: 160,
            iterations: 500,
            max_steps: 20_000,
        };
        let replay = testing::check(config, scenario).unwrap();
        assert_eq!(testing::check(config, scenario).unwrap(), replay);
        assert_eq!(testing::replay(&replay, scenario).unwrap(), replay);
    }
}
