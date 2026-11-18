use std::{
    any::Any,
    collections::BTreeMap,
    fmt,
    future::{Future, poll_fn},
    panic,
    pin::pin,
    sync::Arc,
    task::Poll,
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
            #[expect(
                clippy::disallowed_methods,
                reason = "either Unix signal must trigger scope cancellation"
            )]
            let () = tokio::select! {
                biased;
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            };
            Ok::<(), std::io::Error>(())
        };
        #[cfg(windows)]
        let received = tokio::signal::ctrl_c();

        #[expect(
            clippy::disallowed_methods,
            reason = "scope cancellation ends the signal wait and releases its listeners"
        )]
        let result = tokio::select! {
            biased;
            () = self.cancelled() => Ok(()),
            result = received => {
                result?;
                self.cancel();
                Ok(())
            }
        };
        result
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
    /// Whether reaping found a task cancelled that the scope never aborted.
    /// Its runtime is shutting down, and its result is gone.
    reaped_cancelled: bool,
}

impl<E> Errors<E> {
    /// Whether a child failure has to stop a running body: an error nobody
    /// can claim, or a panic.
    fn must_stop(&self) -> bool {
        self.orphaned || self.panic.is_some()
    }
}

/// What a scope shares with its tasks and their handles.
struct Ledger<E> {
    errors: Lock<Errors<E>>,
    /// Woken when a running body first has to stop.
    on_stop: WaitQueue,
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

fn take_oldest<E>(errors: &SharedErrors<E>) -> Option<E> {
    errors.lock().recorded.pop_first().map(|(_, error)| error)
}

/// Marks that a recorded error can no longer reach anyone through its handle,
/// and wakes a body waiting in [`stopped`].
fn orphan<E>(errors: &SharedErrors<E>) {
    let first = {
        let mut errors = errors.lock();
        let first = !errors.must_stop();
        errors.orphaned = true;
        first
    };
    if first {
        errors.on_stop.notify_all();
    }
}

/// Keeps the first panic among the scope's join errors, and wakes a body
/// waiting in [`stopped`]. Returns whether `join_error` was a panic.
fn keep_panic<E>(errors: &SharedErrors<E>, join_error: JoinError) -> bool {
    let Ok(payload) = join_error.try_into_panic() else {
        return false;
    };
    let first = {
        let mut errors = errors.lock();
        let first = !errors.must_stop();
        if errors.panic.is_none() {
            errors.panic = Some(payload);
        }
        first
    };
    if first {
        errors.on_stop.notify_all();
    }
    true
}

/// Notes what a task taken out of the set early, before the scope joins,
/// left behind. Returns whether it panicked.
fn reaped<E>(errors: &SharedErrors<E>, result: Result<(), JoinError>) -> bool {
    match result {
        Ok(()) => false,
        Err(JoinError::Cancelled) => {
            errors.lock().reaped_cancelled = true;
            false
        }
        Err(join_error) => keep_panic(errors, join_error),
    }
}

/// Waits for a task of `set` to finish and takes it out of the set, or
/// returns `None` once the set is empty.
async fn join_next(set: &Shared<JoinSet>) -> Option<Result<(), JoinError>> {
    poll_fn(|cx| lock(set).poll_join_next(cx)).await
}

/// Takes tasks out of `set` as they finish while a body runs, so the scope
/// lets go of them even if the body never spawns again. Returns once a task
/// has panicked, with the panic kept for the scope to resume.
async fn reap<E>(set: &Shared<JoinSet>, errors: &SharedErrors<E>) {
    poll_fn(|cx| {
        let mut set = lock(set);
        loop {
            match set.poll_join_next(cx) {
                Poll::Ready(Some(result)) => {
                    if reaped(errors, result) {
                        return Poll::Ready(());
                    }
                }
                // An empty set wakes this poll at the next spawn.
                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await
}

/// Reaps finished tasks until a child failure has to stop the body: an
/// error is orphaned, or a task panics, wherever the panic was found.
/// Returns at once if one already has.
async fn stopped<E>(set: &Shared<JoinSet>, errors: &SharedErrors<E>) {
    let must_stop = async {
        loop {
            if errors.lock().must_stop() {
                return;
            }
            // Listen before checking again, so a failure between the two
            // checks still wakes this wait.
            let listener = errors.on_stop.listen();
            if errors.lock().must_stop() {
                return;
            }
            listener.await;
        }
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "the scope stops reaping completed children when a failure is recorded"
    )]
    let () = tokio::select! {
        biased;
        () = must_stop => {},
        () = reap(set, errors) => {},
    };
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

/// Runs a task of [`Tasks::spawn_handled`]: its error goes to `on_error`, in
/// place of the scope's record, so it never fails the scope.
async fn handled<X>(future: impl Future<Output = Result<(), X>>, on_error: impl FnOnce(X)) {
    if let Err(error) = future.await {
        on_error(error);
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
/// scope, as a thread panic is resumed by a scoped thread. Under `run` or
/// `run_until` the panic also stops the body at its next `await`. Dropping a scope
/// without joining aborts its async tasks without waiting for them. Started
/// blocking closures continue running. See [`run_until`](Self::run_until)
/// for cancellation, deadlines, and the cleanup contract.
///
/// Spawning needs a Tokio runtime, as every spawn does. Use
/// [`run`](Self::run) for the common shape, or [`new`](Self::new),
/// [`spawn`](Self::spawn) and [`join`](Self::join) by hand.
pub struct Tasks<E> {
    /// Shared with the loop of [`run`](Self::run) or
    /// [`run_until`](Self::run_until), which reaps finished tasks while the
    /// body holds the scope.
    set: Shared<JoinSet>,
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
            set: shared(JoinSet::new()),
            world: World::current(),
            errors: Arc::new(Ledger {
                errors: Lock::new(Errors {
                    next: 0,
                    recorded: BTreeMap::new(),
                    panic: None,
                    orphaned: false,
                    reaped_cancelled: false,
                }),
                on_stop: WaitQueue::new(),
            }),
            cancellation: None,
            #[cfg(feature = "tracing")]
            named: None,
        }
    }

    /// Runs `body` with a scope, then joins the scope. This is the usual
    /// way to use `Tasks`. An [orphaned](Task) task error stops the body at its
    /// next `await` and becomes the scope's error. A task panic stops the body
    /// the same way: the other tasks are aborted and drained, and the panic is
    /// resumed. Tasks that finish while the body runs are let go of at once,
    /// even if the body spawns nothing more.
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
            let set = Arc::clone(&tasks.set);
            let errors = Arc::clone(&tasks.errors);
            #[expect(
                clippy::disallowed_methods,
                reason = "the task scope observes child failure while its body is pending"
            )]
            let () = tokio::select! {
                biased;
                () = stopped(&set, &errors) => {},
                result = body(&mut tasks) => return tasks.join(result).await,
            };
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
    /// with the oldest unclaimed error. A child panic stops the body the same
    /// way, and is resumed once the other children are drained. A shutdown
    /// request present at the same poll still wins the poll, but that error or
    /// panic then wins the result. Finished children are let go of while the
    /// body runs, as in `run`.
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
            let set = Arc::clone(&tasks.set);
            let errors = Arc::clone(&tasks.errors);
            #[expect(
                clippy::disallowed_methods,
                reason = "run_until observes shutdown and child failure while its body is pending"
            )]
            let reason = tokio::select! {
                biased;
                reason = shutdown(&cancellation, deadline) => reason,
                () = stopped(&set, &errors) => return tasks.fail().await.map_err(ScopeError::Failed),
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
        self.reap();
        let (tx, rx) = Reply::pair();
        let errors = Arc::clone(&self.errors);
        lock(&self.set).spawn(async move {
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
            let future = self.instrument(name.into(), future);
            self.spawn(future)
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = name;
            self.spawn(future)
        }
    }

    /// Starts `future` as a task whose error goes to `on_error` instead of
    /// to the scope. Use it for work whose failure is its own business, such
    /// as one connection of a server: a failed connection is logged or
    /// counted, and the scope and the other connections keep running.
    ///
    /// `on_error` runs in the task, after `future` fails, with the error.
    /// The error never reaches the scope, so it neither stops the body nor
    /// fails the join. There is no handle: nothing waits for the task, and
    /// the task's error type need not be the scope's. A panic, in `future`
    /// or in `on_error`, still fails the scope as any task panic does, and
    /// the task is aborted with the others when the scope ends. Under
    /// [`run`](Self::run) or [`run_until`](Self::run_until) the panic stops
    /// the body as soon as the task ends, so a server whose connection
    /// panics stops then, not at shutdown.
    ///
    /// ```
    /// use adamantium::{Actor, Tasks};
    /// # async fn serve(id: u32) -> Result<(), String> {
    /// #     if id == 2 { Err(format!("connection {id} reset")) } else { Ok(()) }
    /// # }
    /// # #[tokio::main] async fn main() -> Result<(), std::io::Error> {
    /// let failed = Actor::new(0);
    /// Tasks::<std::io::Error>::run(async |tasks| {
    ///     for id in 1..=3 {
    ///         let failed = failed.clone();
    ///         tasks.spawn_handled(serve(id), move |error| {
    ///             eprintln!("{error}");
    ///             failed.with(|failed| *failed += 1);
    ///         });
    ///     }
    ///     Ok(())
    /// })
    /// .await?;
    /// assert_eq!(failed.read(|failed| *failed), 1);
    /// # Ok(()) }
    /// ```
    pub fn spawn_handled<X, F, H>(&mut self, future: F, on_error: H)
    where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.world.check();
        self.reap();
        lock(&self.set).spawn(handled(future, on_error));
    }

    /// Starts a named task whose error goes to `on_error`, with the contract
    /// of [`spawn_handled`](Self::spawn_handled). With the `tracing` feature,
    /// the task, `on_error` included, runs inside an `adamantium.task` span
    /// as for [`spawn_named`](Self::spawn_named), and is reported at scope
    /// deadlines until it ends. Without `tracing`, this behaves as
    /// `spawn_handled` without converting or storing the name.
    pub fn spawn_handled_named<X, F, H>(
        &mut self,
        name: impl Into<crate::Str>,
        future: F,
        on_error: H,
    ) where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.world.check();
        #[cfg(feature = "tracing")]
        {
            self.reap();
            let task = self.instrument(name.into(), handled(future, on_error));
            lock(&self.set).spawn(task);
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = name;
            self.spawn_handled(future, on_error);
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
        self.reap();
        let (tx, rx) = Reply::pair();
        let errors = Arc::clone(&self.errors);
        lock(&self.set).spawn_blocking(move || {
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
            #[expect(
                clippy::disallowed_methods,
                reason = "shutdown interrupts normal settling so the scope can abort and drain"
            )]
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
            if !aborted && (body_failed || self.failed()) {
                self.abort();
                aborted = true;
            }
            match join_next(&self.set).await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => self.remember_panic(join_error),
                None => break,
            }
        }
        self.propagate_panic();
    }

    /// Ends a scope whose body an orphaned error or a task panic stopped.
    async fn fail<R>(mut self) -> Result<R, E> {
        // A panic is resumed here, once the other tasks are drained.
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

    /// The number of unfinished tasks, including work being dropped on abort.
    pub fn len(&self) -> usize {
        self.world.check();
        lock(&self.set).unfinished()
    }

    /// Whether every spawned task has finished.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn reap(&mut self) {
        loop {
            let next = lock(&self.set).try_join_next();
            let Some(result) = next else {
                break;
            };
            if reaped(&self.errors, result) {
                self.abort();
            }
        }
    }

    fn failed(&self) -> bool {
        let errors = self.errors.lock();
        errors.reaped_cancelled || !errors.recorded.is_empty() || errors.panic.is_some()
    }

    async fn resume_first_panic<T>(&mut self) -> T {
        let reaped_failure = {
            let errors = self.errors.lock();
            errors.reaped_cancelled || errors.panic.is_some()
        };
        if reaped_failure {
            self.abort();
            self.drain().await;
        }
        while let Some(next) = join_next(&self.set).await {
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
        if keep_panic(&self.errors, join_error) {
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
        lock(&self.set).abort_all();
    }

    async fn drain(&mut self) {
        while let Some(next) = join_next(&self.set).await {
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
            .field("len", &lock(&self.set).len())
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
        while !tasks.failed() {
            if lock(&tasks.set).len() < limit
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
            match join_next(&tasks.set).await {
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
        while lock(&outcomes).winner.is_none() && !tasks.failed() {
            if lock(&tasks.set).len() < limit
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
            match join_next(&tasks.set).await {
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

#[cfg(test)]
mod retention_tests {
    use super::*;
    use futures_util::FutureExt;
    use std::panic::AssertUnwindSafe;

    async fn yield_now() {
        #[cfg(feature = "deterministic")]
        if World::current().is_model() {
            shuttle::future::yield_now().await;
            return;
        }
        tokio::task::yield_now().await;
    }

    struct Dropped(crate::Actor<bool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.with(|dropped| *dropped = true);
        }
    }

    enum Reap {
        Spawn,
        SpawnBlocking,
    }

    #[tokio::test]
    async fn aborted_work_stays_unfinished_until_it_is_dropped() {
        let mut tasks = Tasks::<()>::new();
        tasks.spawn(std::future::pending::<Result<(), ()>>());
        tasks.spawn(std::future::pending::<Result<(), ()>>());
        assert_eq!(tasks.len(), 2);
        tasks.abort();
        assert_eq!(tasks.len(), 2);
        tasks.drain().await;
        assert!(tasks.is_empty());
    }

    async fn panic_scenario() {
        for blocking in [false, true] {
            for through_value in [false, true] {
                for reaper in [Reap::Spawn, Reap::SpawnBlocking] {
                    let mut tasks = Tasks::<&str>::new();
                    let failed = if blocking {
                        tasks.spawn_blocking::<(), _>(|| panic!("reaped panic"))
                    } else {
                        tasks.spawn::<(), _>(async { panic!("reaped panic") })
                    };
                    while !failed.rx.is_closed() {
                        yield_now().await;
                    }
                    while tasks.errors.lock().panic.is_none() {
                        match reaper {
                            Reap::Spawn => drop(tasks.spawn(async { Ok(()) })),
                            Reap::SpawnBlocking => drop(tasks.spawn_blocking(|| Ok(()))),
                        }
                        yield_now().await;
                    }

                    // Work added after reaping must also be drained before the
                    // saved panic is resumed.
                    let dropped = crate::Actor::new(false);
                    let guard = Dropped(dropped.clone());
                    tasks.spawn(async move {
                        let _guard = guard;
                        std::future::pending::<()>().await;
                        Ok(())
                    });
                    let panic = AssertUnwindSafe(async move {
                        if through_value {
                            tasks.value(failed).await
                        } else {
                            tasks.join(Err::<(), _>("body error")).await
                        }
                    })
                    .catch_unwind()
                    .await
                    .unwrap_err();
                    assert_eq!(panic.downcast_ref::<&str>(), Some(&"reaped panic"));
                    assert!(dropped.snapshot());
                }
            }
        }
    }

    #[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
    #[tokio::test]
    async fn reaped_panics_reach_value_and_join_after_cleanup() {
        #[allow(clippy::disallowed_methods, reason = "test watchdog")]
        tokio::time::timeout(std::time::Duration::from_secs(30), panic_scenario())
            .await
            .unwrap();
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn reaped_panics_survive_deterministic_replay() {
        let config = crate::testing::Config {
            seed: 187,
            iterations: 100,
            max_steps: 30_000,
        };
        let replay = crate::testing::check(config, panic_scenario).unwrap();
        assert_eq!(
            crate::testing::replay(&replay, panic_scenario).unwrap(),
            replay
        );
    }

    #[tokio::test]
    async fn spawning_keeps_retained_entries_bounded_across_100_000_tasks() {
        Tasks::<()>::run(async |tasks| {
            for _ in 0..1_000 {
                let handles: Vec<_> = (0..100).map(|_| tasks.spawn(async { Ok(()) })).collect();
                assert!(
                    lock(&tasks.set).len() <= 100,
                    "completed entries accumulated"
                );
                for handle in handles {
                    tasks.value(handle).await?;
                }
            }
            assert_eq!(tasks.len(), 0);
            for _ in 0..1_000 {
                for _ in 0..100 {
                    tasks.spawn_handled(async { Ok::<_, ()>(()) }, |()| {});
                }
                assert!(
                    lock(&tasks.set).len() <= 100,
                    "completed handled entries accumulated"
                );
                while !tasks.is_empty() {
                    yield_now().await;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    /// Spawns connections that finish at once, then waits without spawning
    /// again: only the running scope's own reaping can let go of them.
    async fn idle_server_scenario() {
        Tasks::<()>::run(async |tasks| {
            let served = crate::Actor::new(0);
            for _ in 0..100 {
                let served = served.clone();
                tasks.spawn_handled(
                    async move {
                        served.with(|served| *served += 1);
                        Ok::<_, ()>(())
                    },
                    |()| panic!("the connection succeeded"),
                );
            }
            while served.snapshot() < 100 || !lock(&tasks.set).is_empty() {
                crate::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_running_scope_reaps_tasks_without_another_spawn() {
        #[allow(clippy::disallowed_methods, reason = "test watchdog")]
        tokio::time::timeout(std::time::Duration::from_secs(30), idle_server_scenario())
            .await
            .unwrap();
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn a_running_scope_reaps_tasks_without_another_spawn_under_replay() {
        let config = crate::testing::Config {
            seed: 195,
            iterations: 50,
            max_steps: 100_000,
        };
        let replay = crate::testing::check(config, idle_server_scenario).unwrap();
        assert_eq!(
            crate::testing::replay(&replay, idle_server_scenario).unwrap(),
            replay
        );
    }

    /// The scope's loop and the next spawn race to reap the same panic.
    /// Whichever finds it, the body stops and the sibling is drained.
    async fn panic_reaped_by_either_scenario() {
        let dropped = crate::Actor::new(false);
        let guard = Dropped(dropped.clone());
        let scope = Tasks::<()>::run(async move |tasks| {
            tasks.spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            });
            let failed = tasks.spawn::<(), _>(async { panic!("reaped panic") });
            while !failed.rx.is_closed() {
                yield_now().await;
            }
            drop(tasks.spawn(async { Ok(()) }));
            std::future::pending::<Result<(), ()>>().await
        });
        let panic = AssertUnwindSafe(scope).catch_unwind().await.unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"reaped panic"));
        assert!(dropped.snapshot());
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn a_panic_reaped_by_a_spawn_still_stops_the_body() {
        let config = crate::testing::Config {
            seed: 195,
            iterations: 200,
            max_steps: 30_000,
        };
        let replay = crate::testing::check(config, panic_reaped_by_either_scenario).unwrap();
        assert_eq!(
            crate::testing::replay(&replay, panic_reaped_by_either_scenario).unwrap(),
            replay
        );
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn finished_tasks_are_reaped_across_many_modeled_batches() {
        async fn scenario() {
            Tasks::<()>::run(async |tasks| {
                for _ in 0..100 {
                    let retained = tasks.spawn(async { Ok(42) });
                    let blocking = tasks.spawn_blocking(|| Ok(7));
                    while !tasks.is_empty() {
                        yield_now().await;
                    }
                    let trigger = tasks.spawn(async { Ok(()) });
                    assert_eq!(lock(&tasks.set).len(), 1);
                    // Reaping only releases the scope's bookkeeping. Handles
                    // continue to own their results until value is requested.
                    assert_eq!(tasks.value(retained).await, Ok(42));
                    assert_eq!(tasks.value(blocking).await, Ok(7));
                    assert_eq!(tasks.value(trigger).await, Ok(()));
                }
                Ok(())
            })
            .await
            .unwrap();
        }
        let config = crate::testing::Config {
            seed: 187,
            iterations: 20,
            max_steps: 100_000,
        };
        let replay = crate::testing::check(config, scenario).unwrap();
        assert_eq!(crate::testing::replay(&replay, scenario).unwrap(), replay);
    }
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
            while !tasks.failed() {
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
