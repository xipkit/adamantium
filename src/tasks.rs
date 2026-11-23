use std::{
    any::Any,
    collections::BTreeMap,
    fmt,
    future::{Future, poll_fn},
    panic,
    pin::pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use crate::runtime::{
    Guard, Lock, World,
    task::{JoinError, JoinSet},
    wait::WaitQueue,
};
use tokio::time::Instant;

use crate::{Closed, FeedError, ReceiveError, Reply, ReplyReceiver, UntilError, Watch, time};

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

    // Drop calls this directly to avoid a second panic from cancel's ownership check.
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
            // Shutdown checks must see a request even at the end of a task's
            // budget. This watch only ever changes from false to true.
            if !requested.get() {
                requested.changed_unbudgeted().await;
            }
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
        self.world.check_unless_panicking();
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

#[derive(Debug, PartialEq, Eq)]
pub enum ScopeError<E> {
    Failed(E),
    Cancelled,
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

    pub(crate) fn receive_error(self) -> ReceiveError {
        match self {
            Self::Cancelled => ReceiveError::Cancelled,
            Self::DeadlineExceeded => ReceiveError::DeadlineExceeded,
        }
    }

    pub(crate) fn feed_error(self) -> FeedError {
        match self {
            Self::Cancelled => FeedError::Cancelled,
            Self::DeadlineExceeded => FeedError::DeadlineExceeded,
        }
    }

    pub(crate) fn until_error(self) -> UntilError {
        match self {
            Self::Cancelled => UntilError::Cancelled,
            Self::DeadlineExceeded => UntilError::DeadlineExceeded,
        }
    }
}

// Waits for `future` unless shutdown is requested first. A request present at
// any poll wins over a ready future, which is then dropped unpolled.
#[allow(
    clippy::future_not_send,
    reason = "the future is Send when the wrapped future is"
)]
pub(crate) async fn before_shutdown<Fut: Future>(
    cancellation: &Cancellation,
    deadline: Option<Instant>,
    future: Fut,
) -> Result<Fut::Output, Shutdown> {
    #[expect(
        clippy::disallowed_methods,
        reason = "shutdown is checked before the future is polled"
    )]
    let outcome = tokio::select! {
        biased;
        reason = shutdown(cancellation, deadline) => Err(reason),
        output = future => Ok(output),
    };
    outcome
}

pub(crate) async fn shutdown(cancellation: &Cancellation, deadline: Option<Instant>) -> Shutdown {
    let mut cancelled = pin!(cancellation.cancelled());
    let mut elapsed = pin!(elapsed(deadline));
    std::future::poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Shutdown::Cancelled);
        }
        if elapsed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Shutdown::DeadlineExceeded);
        }
        Poll::Pending
    })
    .await
}

async fn elapsed(deadline: Option<Instant>) {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    let mut sleep = pin!(time::sleep_until(deadline));
    // Tokio rounds timers up. Check the clock so expired deadlines win this poll.
    poll_fn(|cx| {
        if deadline <= time::now() || sleep.as_mut().poll(cx).is_ready() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}

// Record errors even when no handle remains. Keep an observed panic until
// cleanup finishes, including when cancellation interrupts the drain.
struct Errors<E> {
    next: u64,
    recorded: BTreeMap<u64, E>,
    panic: Option<Box<dyn Any + Send>>,
    orphaned: bool,
    // Whether reaping found a task cancelled that the scope never aborted.
    // Its runtime is shutting down, and its result is gone.
    reaped_cancelled: bool,
}

impl<E> Errors<E> {
    fn must_stop(&self) -> bool {
        self.orphaned || self.panic.is_some()
    }

    fn reaped_failure(&self) -> bool {
        self.reaped_cancelled || self.panic.is_some()
    }

    fn failed(&self) -> bool {
        self.reaped_failure() || !self.recorded.is_empty()
    }
}

struct Ledger<E> {
    errors: Lock<Errors<E>>,
    on_stop: WaitQueue,
}

impl<E> Ledger<E> {
    fn new() -> Self {
        Self {
            errors: Lock::new(Errors {
                next: 0,
                recorded: BTreeMap::new(),
                panic: None,
                orphaned: false,
                reaped_cancelled: false,
            }),
            on_stop: WaitQueue::named("tasks.stop"),
        }
    }

    fn lock(&self) -> Guard<'_, Errors<E>> {
        self.errors.lock("Ledger::lock")
    }

    fn record(&self, error: E) -> Receipt {
        let mut errors = self.lock();
        let place = errors.next;
        errors.next += 1;
        errors.recorded.insert(place, error);
        Receipt(place)
    }

    fn claim(&self, receipt: Receipt) -> Option<E> {
        self.lock().recorded.remove(&receipt.0)
    }

    fn take_oldest(&self) -> Option<E> {
        self.lock().recorded.pop_first().map(|(_, error)| error)
    }

    // Wakes the scope only for the first reason to stop, after the lock is
    // released.
    fn stop_with(&self, mark: impl FnOnce(&mut Errors<E>)) {
        let first = {
            let mut errors = self.lock();
            let first = !errors.must_stop();
            mark(&mut errors);
            first
        };
        if first {
            self.on_stop.notify_all();
        }
    }

    fn orphan(&self) {
        self.stop_with(|errors| errors.orphaned = true);
    }

    fn keep_panic(&self, join_error: JoinError) -> bool {
        let Ok(payload) = join_error.try_into_panic() else {
            return false;
        };
        // A payload that is not kept is dropped after the lock is released.
        let mut payload = Some(payload);
        self.stop_with(|errors| {
            if errors.panic.is_none() {
                errors.panic = payload.take();
            }
        });
        true
    }

    fn reaped(&self, result: Result<(), JoinError>) -> bool {
        match result {
            Ok(()) => false,
            Err(JoinError::Cancelled) => {
                self.lock().reaped_cancelled = true;
                false
            }
            Err(join_error) => self.keep_panic(join_error),
        }
    }
}

struct Receipt(u64);

type Locked<V> = Arc<Lock<V>>;

fn locked<V>(value: V) -> Locked<V> {
    Arc::new(Lock::new(value))
}

#[allow(
    clippy::expect_used,
    reason = "each registered task starts at most once"
)]
fn take_work<W>(task: Locked<Option<W>>) -> W {
    task.lock("Tasks::take_work")
        .take()
        .expect("task starts once")
}

// If runtime cancellation left the caller holding the last reference, preserve
// the task's destructor panic just as the runtime would have done.
fn release_work<E, W>(ledger: &Ledger<E>, retained: Locked<Option<W>>) {
    if let Err(payload) = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(retained))) {
        ledger.keep_panic(JoinError::Panic(payload));
    }
}

// Who submits work. The owner is never refused; a handle is, once the scope
// has closed admission.
#[derive(Clone, Copy)]
enum Submitter {
    Owner,
    Handle,
}

struct Registering<'a> {
    set: &'a Locked<JoinSet>,
    admitted: bool,
}

impl Drop for Registering<'_> {
    fn drop(&mut self) {
        if self.admitted {
            self.set.lock("JoinSet::finish_spawn").finish_spawn();
        }
    }
}

async fn join_next(set: &Locked<JoinSet>) -> Option<Result<(), JoinError>> {
    poll_fn(|cx| set.lock("JoinSet::poll_join_next").poll_join_next(cx)).await
}

async fn reap<E>(set: &Locked<JoinSet>, ledger: &Ledger<E>) {
    poll_fn(|cx| {
        loop {
            let next = set.lock("JoinSet::poll_join_next").poll_join_next(cx);
            match next {
                Poll::Ready(Some(result)) => {
                    if ledger.reaped(result) {
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

async fn stopped<E>(set: &Locked<JoinSet>, ledger: &Ledger<E>) {
    let must_stop = async {
        loop {
            if ledger.lock().must_stop() {
                return;
            }
            // Register before the second check so a concurrent failure wakes the wait.
            let listener = ledger.on_stop.listen();
            if ledger.lock().must_stop() {
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
        () = reap(set, ledger) => {},
    };
}

// Sends a task's result to its handle. An error the handle is no longer
// there to take stays recorded, now orphaned.
fn deliver<T, E>(ledger: &Ledger<E>, tx: Reply<Result<T, Receipt>>, result: Result<T, Receipt>) {
    if let Err(Closed(Err(_))) = tx.send(result) {
        ledger.orphan();
    }
}

async fn handled<X>(future: impl Future<Output = Result<(), X>>, on_error: impl FnOnce(X)) {
    if let Err(error) = future.await {
        on_error(error);
    }
}

#[allow(
    clippy::expect_used,
    reason = "the scope owner does not check admission"
)]
fn owner_submission<T>(result: Result<T, SpawnError>) -> T {
    result.expect("owner submission cannot be rejected")
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
/// for cancellation, deadlines, and the cleanup contract, and
/// [`run_with_grace`](Self::run_with_grace) for a shutdown that lets admitted
/// tasks finish first.
///
/// Spawning needs a Tokio runtime, as every spawn does. Use
/// [`run`](Self::run) for the common shape, or [`new`](Self::new),
/// [`spawn`](Self::spawn) and [`join`](Self::join) by hand.
pub struct Tasks<E> {
    spawner: Spawner<E>,
    cancellation: Option<Cancellation>,
}

/// A cloneable capability to add tasks to one [`Tasks`] scope.
///
/// Obtain one with [`Tasks::spawner`]. Registered children can add siblings
/// even after the body returns successfully. Idle handles do not keep the
/// scope alive: joining closes admission when the last registered task has
/// been drained. Failure shutdown and owner drop also close admission.
/// A spawn racing closure either joins the scope or returns [`SpawnError`].
///
/// Only `Tasks` can join, cancel, or claim task results with [`Tasks::value`].
/// This handle does not limit concurrency or provide sibling-to-sibling waits.
pub struct Spawner<E> {
    set: Locked<JoinSet>,
    world: World,
    ledger: Arc<Ledger<E>>,
    #[cfg(feature = "tracing")]
    named: Locked<trace::Running>,
}

/// The scope has closed admission and the submitted work was not started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnError;

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task scope is closed")
    }
}

impl std::error::Error for SpawnError {}

impl<E: Send + 'static> Spawner<E> {
    /// Starts `future` in this scope, or drops it without polling if the
    /// scope has closed admission. Dropping rejected work happens outside
    /// the admission lock, so its cleanup may safely attempt another spawn.
    ///
    /// Errors and panics follow [`Tasks::spawn`]'s rules. The returned handle
    /// can be sent to the owner for [`Tasks::value`], or dropped to leave its
    /// result to the scope. `SpawnError` is returned to the caller rather
    /// than recorded as a task failure.
    pub fn spawn<T, F>(&self, future: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.submit_future(Submitter::Handle, future)
    }

    /// Starts a named task as in [`Tasks::spawn_named`], with tracing spans
    /// and deterministic timeline names when the corresponding features are enabled.
    pub fn spawn_named<T, F>(
        &self,
        name: impl Into<crate::Str>,
        future: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawn(self.name_future(name, future))
    }

    /// Starts a task whose error goes to the handler, as in [`Tasks::spawn_handled`].
    /// If admission has closed, neither the future nor the handler runs.
    pub fn spawn_handled<X, F, H>(&self, future: F, on_error: H) -> Result<(), SpawnError>
    where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.submit_handled(Submitter::Handle, handled(future, on_error))
    }

    /// Starts a named task whose error goes to the handler, as in [`Tasks::spawn_handled_named`].
    pub fn spawn_handled_named<X, F, H>(
        &self,
        name: impl Into<crate::Str>,
        future: F,
        on_error: H,
    ) -> Result<(), SpawnError>
    where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.submit_handled(
            Submitter::Handle,
            self.name_future(name, handled(future, on_error)),
        )
    }

    /// Registers blocking work as in [`Tasks::spawn_blocking`].
    /// Scope shutdown waits for admitted closures that have started.
    pub fn spawn_blocking<T, F>(&self, f: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.submit_blocking(Submitter::Handle, f)
    }

    /// Registers named blocking work as in [`Tasks::spawn_blocking_named`].
    pub fn spawn_blocking_named<T, F>(
        &self,
        name: impl Into<crate::Str>,
        f: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.spawn_blocking(self.name_blocking(name, f))
    }

    fn register<W: Send + 'static>(
        &self,
        submitter: Submitter,
        work: W,
        submit: impl FnOnce(&mut JoinSet, Locked<Option<W>>),
    ) -> Result<(), SpawnError> {
        self.world.check();
        // Keep registration alive until any synchronously cancelled work
        // has been destroyed outside the set lock.
        let mut registering = Registering {
            set: &self.set,
            admitted: false,
        };
        let retained = locked(Some(work));
        {
            let mut set = self.set.lock("Tasks::submit");
            // The owner already reaped and may still submit after observing
            // a panic, so it can drain the new work before resuming the panic.
            if matches!(submitter, Submitter::Handle) {
                if !set.begin_spawn() {
                    return Err(SpawnError);
                }
                registering.admitted = true;
                set.reap_finished();
            }
            submit(&mut set, Arc::clone(&retained));
        }
        release_work(&self.ledger, retained);
        Ok(())
    }

    fn submit_future<T, F>(&self, submitter: Submitter, future: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let ledger = Arc::clone(&self.ledger);
        self.submit_handled(submitter, async move {
            let result = future.await.map_err(|error| ledger.record(error));
            deliver(&ledger, tx, result);
        })?;
        Ok(self.task(rx))
    }

    fn submit_handled(
        &self,
        submitter: Submitter,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), SpawnError> {
        self.register(submitter, future, |set, work| {
            set.spawn(async move { take_work(work).await });
        })
    }

    fn submit_blocking<T, F>(&self, submitter: Submitter, f: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let ledger = Arc::clone(&self.ledger);
        self.register(
            submitter,
            move || {
                let result = f().map_err(|error| ledger.record(error));
                deliver(&ledger, tx, result);
            },
            |set, work| set.spawn_blocking(move || take_work(work)()),
        )?;
        Ok(self.task(rx))
    }

    fn task<T>(&self, rx: ReplyReceiver<Result<T, Receipt>>) -> Task<T, E> {
        Task {
            world: self.world,
            rx,
            ledger: Arc::clone(&self.ledger),
        }
    }

    fn name_future<F: Future + Send + 'static, N: Into<crate::Str>>(
        &self,
        name: N,
        future: F,
    ) -> impl Future<Output = F::Output> + Send + 'static + use<F, N, E> {
        self.world.check();
        #[cfg(feature = "tracing")]
        let name = name.into();
        #[cfg(all(feature = "tracing", feature = "deterministic"))]
        let future = crate::runtime::trace::named(self.world, name.clone(), future);
        #[cfg(feature = "tracing")]
        let future = trace::instrument(&self.named, name, future);
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        let future = crate::runtime::trace::named(self.world, name, future);
        #[cfg(all(not(feature = "tracing"), not(feature = "deterministic")))]
        let _ = name;
        future
    }

    fn name_blocking<T, F, N: Into<crate::Str>>(
        &self,
        name: N,
        f: F,
    ) -> impl FnOnce() -> Result<T, E> + Send + 'static + use<T, F, N, E>
    where
        T: 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        #[cfg(feature = "tracing")]
        let name = name.into();
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        let name = crate::runtime::trace::timeline_name(self.world, || name.into());
        #[cfg(feature = "tracing")]
        {
            #[cfg(feature = "deterministic")]
            let trace_name = crate::runtime::trace::timeline_name(self.world, || name.clone());
            let (running, span) = trace::track(&self.named, name);
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            move || {
                #[cfg(feature = "deterministic")]
                if let Some(trace_name) = trace_name {
                    crate::runtime::trace::record_current_task_name(&trace_name);
                }
                tracing::dispatcher::with_default(&dispatch, move || {
                    span.in_scope(move || {
                        let _running = running;
                        f()
                    })
                })
            }
        }
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        {
            move || {
                if let Some(name) = name {
                    crate::runtime::trace::record_current_task_name(&name);
                }
                f()
            }
        }
        #[cfg(all(not(feature = "tracing"), not(feature = "deterministic")))]
        {
            let _ = name;
            f
        }
    }
}

impl<E> Clone for Spawner<E> {
    fn clone(&self) -> Self {
        self.world.check();
        Self {
            set: Arc::clone(&self.set),
            world: self.world,
            ledger: Arc::clone(&self.ledger),
            #[cfg(feature = "tracing")]
            named: Arc::clone(&self.named),
        }
    }
}

impl<E> fmt::Debug for Spawner<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.world.check();
        f.debug_struct("Spawner").finish_non_exhaustive()
    }
}

#[cfg(feature = "deterministic")]
impl<E> Drop for Spawner<E> {
    fn drop(&mut self) {
        self.world.check_unless_panicking();
    }
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
    ledger: Arc<Ledger<E>>,
}

impl<E: Send + 'static> Tasks<E> {
    /// Gives children permission to add siblings to this scope. Keeping a
    /// spawner alive does not delay joining or shutdown.
    ///
    /// ```
    /// use adamantium::{Actor, Tasks};
    /// # #[tokio::main] async fn main() -> Result<(), &'static str> {
    /// let visited = Actor::new(Vec::new());
    /// Tasks::<&'static str>::run(async |tasks| {
    ///     let spawner = tasks.spawner();
    ///     let visited = visited.clone();
    ///     tasks.spawn(async move {
    ///         for page in ["home", "about", "contact"] {
    ///             let visited = visited.clone();
    ///             spawner.spawn(async move {
    ///                 visited.with(|pages| pages.push(page));
    ///                 Ok(())
    ///             }).map_err(|_| "scope closed")?;
    ///         }
    ///         Ok(())
    ///     });
    ///     Ok(())
    /// }).await?;
    /// assert_eq!(visited.read(Vec::len), 3);
    /// # Ok(()) }
    /// ```
    pub fn spawner(&self) -> Spawner<E> {
        self.spawner.clone()
    }

    /// An empty scope.
    pub fn new() -> Self {
        Self {
            spawner: Spawner {
                set: locked(JoinSet::new()),
                world: World::current(),
                ledger: Arc::new(Ledger::new()),
                #[cfg(feature = "tracing")]
                named: locked(trace::Running::default()),
            },
            cancellation: None,
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
            let set = Arc::clone(&tasks.spawner.set);
            let ledger = Arc::clone(&tasks.spawner.ledger);
            #[expect(
                clippy::disallowed_methods,
                reason = "the task scope observes child failure while its body is pending"
            )]
            let () = tokio::select! {
                biased;
                () = stopped(&set, &ledger) => {},
                result = body(&mut tasks) => return tasks.join(result).await,
            };
            tasks.fail().await
        })
        .await
    }

    /// Runs a scope until completion, cancellation, or an absolute deadline.
    /// Build deadlines with [`crate::time`] or pass `None` for no deadline.
    ///
    /// Each poll checks cancellation and then the deadline before polling the
    /// body. A child panic or orphaned error also stops the body. Shutdown
    /// cancels the signal, aborts async children, and waits for their
    /// destructors and any started [`Self::spawn_blocking`] closures.
    ///
    /// In order of precedence, the result is a child panic resumed after
    /// cleanup, a returned body error, the oldest unclaimed child error, or
    /// the shutdown reason selected when cleanup began.
    ///
    /// Dropping the started future or panicking in its body requests abort
    /// without waiting. To ensure cleanup, request cancellation and await
    /// this future to completion.
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
            let set = Arc::clone(&tasks.spawner.set);
            let ledger = Arc::clone(&tasks.spawner.ledger);
            #[expect(
                clippy::disallowed_methods,
                reason = "run_until observes shutdown and child failure while its body is pending"
            )]
            let reason = tokio::select! {
                biased;
                reason = shutdown(&cancellation, deadline) => reason,
                () = stopped(&set, &ledger) => return tasks.fail().await.map_err(ScopeError::Failed),
                result = body(&mut tasks) => return tasks.join_until(cancellation, deadline, result).await,
            };
            tasks.report_deadline(&reason);
            tasks.abort();
            tasks.join(Ok(())).await.map_err(ScopeError::Failed)?;
            Err(reason.scope_error())
        })
        .await
    }

    /// Runs a scope that gives its children time to finish on cancellation.
    ///
    /// Before cancellation this behaves like [`Self::run_until`] without a
    /// deadline. A cancellation request drops the body and starts the grace
    /// period. Queued blocking work can still start and [`Spawner`] can add
    /// children until the drain ends. Tasks that admit work should observe
    /// the cancellation signal.
    ///
    /// If all tasks finish in time, returns [`ScopeError::Cancelled`]. At the
    /// deadline, aborts remaining tasks and waits for cleanup as in
    /// `run_until`, then returns [`ScopeError::DeadlineExceeded`]. The
    /// deadline wins a tie, zero grace aborts immediately, and an
    /// unrepresentable deadline never expires.
    ///
    /// Failures end the drain early and take precedence as in `run_until`.
    /// Errors handled by [`Self::spawn_handled`] let the drain continue.
    /// Dropping the started future requests abort without waiting.
    ///
    /// ```
    /// use adamantium::{Cancellation, ScopeError, Tasks, time};
    /// use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let shutdown = Cancellation::new();
    /// let request = shutdown.clone();
    /// let result = Tasks::<std::io::Error>::run_with_grace(
    ///     shutdown,
    ///     Duration::from_secs(10),
    ///     async |tasks| {
    ///         // An admitted request that takes a while to answer.
    ///         tasks.spawn(async {
    ///             time::sleep(Duration::from_millis(10)).await;
    ///             Ok(())
    ///         });
    ///         request.cancel();
    ///         // The body stops accepting here; the request above finishes.
    ///         std::future::pending().await
    ///     },
    /// ).await;
    /// assert!(matches!(result, Err::<(), _>(ScopeError::Cancelled)));
    /// # }
    /// ```
    #[allow(clippy::future_not_send, reason = "see run")]
    pub async fn run_with_grace<R, F>(
        cancellation: Cancellation,
        grace: Duration,
        body: F,
    ) -> Result<R, ScopeError<E>>
    where
        R: Send,
        F: AsyncFnOnce(&mut Tasks<E>) -> Result<R, E>,
    {
        crate::runtime::in_world(World::current(), async move {
            let mut tasks = Self::new();
            tasks.cancellation = Some(cancellation.clone());
            let set = Arc::clone(&tasks.spawner.set);
            let ledger = Arc::clone(&tasks.spawner.ledger);
            #[expect(
                clippy::disallowed_methods,
                reason = "run_with_grace observes shutdown and child failure while its body is pending"
            )]
            let body = tokio::select! {
                biased;
                () = cancellation.cancelled() => None,
                () = stopped(&set, &ledger) => return tasks.fail().await.map_err(ScopeError::Failed),
                result = body(&mut tasks) => Some(result),
            };
            let Some(body) = body else {
                let reason = tasks.drain_with_grace(grace, false).await;
                return match tasks.spawner.ledger.take_oldest() {
                    Some(error) => Err(ScopeError::Failed(error)),
                    None => Err(reason.scope_error()),
                };
            };
            #[expect(
                clippy::disallowed_methods,
                reason = "shutdown turns normal settling into a bounded drain"
            )]
            let requested = tokio::select! {
                biased;
                () = cancellation.cancelled() => true,
                () = tasks.settle(body.is_err()) => false,
            };
            let reason = if requested {
                Some(tasks.drain_with_grace(grace, body.is_err()).await)
            } else {
                None
            };
            tasks.outcome(body, reason)
        })
        .await
    }

    /// Starts `future` as a task of this scope and returns its handle.
    pub fn spawn<T, F>(&mut self, future: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawner.world.check();
        self.reap();
        owner_submission(self.spawner.submit_future(Submitter::Owner, future))
    }

    /// Starts a named task. With the `tracing` feature, its future is polled
    /// inside an `adamantium.task` span with a `task.name` field, parented to
    /// the current span. Unfinished named tasks are reported at scope deadlines.
    /// Deterministic timeline capture also records the name. When neither
    /// tracing nor timeline capture is active, the name is unused.
    pub fn spawn_named<T, F>(&mut self, name: impl Into<crate::Str>, future: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let future = self.spawner.name_future(name, future);
        self.spawn(future)
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
        self.spawner.world.check();
        self.reap();
        owner_submission(
            self.spawner
                .submit_handled(Submitter::Owner, handled(future, on_error)),
        );
    }

    /// Starts a named task whose error goes to `on_error`, with the contract
    /// of [`spawn_handled`](Self::spawn_handled). With the `tracing` feature,
    /// the task, `on_error` included, runs inside an `adamantium.task` span
    /// as for [`spawn_named`](Self::spawn_named), and is reported at scope
    /// deadlines until it ends. Deterministic timeline capture also records
    /// the name.
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
        let future = self.spawner.name_future(name, handled(future, on_error));
        self.reap();
        owner_submission(self.spawner.submit_handled(Submitter::Owner, future));
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
        self.spawner.world.check();
        self.reap();
        owner_submission(self.spawner.submit_blocking(Submitter::Owner, f))
    }

    /// Registers named blocking work, with the same cleanup contract as
    /// [`spawn_blocking`](Self::spawn_blocking). With `tracing`, the closure
    /// runs inside an `adamantium.task` span and is included in deadline reports
    /// until it finishes. Deterministic timeline capture also records the name.
    pub fn spawn_blocking_named<T, F>(&mut self, name: impl Into<crate::Str>, f: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let f = self.spawner.name_blocking(name, f);
        self.spawn_blocking(f)
    }

    /// Waits for a task and returns its result. If the task panicked, the
    /// panic is resumed here.
    pub async fn value<T: Send>(&mut self, task: Task<T, E>) -> Result<T, E> {
        crate::runtime::in_world(self.spawner.world, async move {
            let error = match task.rx.receive().await {
                Some(Ok(value)) => return Ok(value),
                Some(Err(receipt)) => task.ledger.claim(receipt),
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
        crate::runtime::in_world(self.spawner.world, async move {
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
        self.spawner.world.check();
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
                self.abort_and_drain(reason).await;
            }
            self.outcome(body, reason)
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
            let next = poll_fn(|cx| {
                let mut set = self.spawner.set.lock("Tasks::settle");
                let next = set.poll_join_next(cx);
                if matches!(next, Poll::Ready(None)) {
                    set.close();
                }
                next
            })
            .await;
            match next {
                Some(Ok(())) => {}
                Some(Err(join_error)) => self.remember_panic(join_error),
                None => break,
            }
        }
        self.propagate_panic();
    }

    async fn drain_with_grace(&mut self, grace: Duration, body_failed: bool) -> Shutdown {
        let deadline = time::now().checked_add(grace);
        #[expect(
            clippy::disallowed_methods,
            reason = "the grace deadline interrupts normal settling so the scope can abort and drain"
        )]
        let expired = tokio::select! {
            biased;
            () = elapsed(deadline) => true,
            () = self.settle(body_failed) => false,
        };
        if !expired {
            return Shutdown::Cancelled;
        }
        let reason = Shutdown::DeadlineExceeded;
        self.abort_and_drain(&reason).await;
        reason
    }

    async fn abort_and_drain(&mut self, reason: &Shutdown) {
        self.report_deadline(reason);
        self.abort();
        self.drain().await;
    }

    #[cfg(not(feature = "tracing"))]
    fn report_deadline(&self, _reason: &Shutdown) {}

    async fn fail<R>(mut self) -> Result<R, E> {
        self.settle(true).await;
        match self.spawner.ledger.take_oldest() {
            Some(error) => Err(error),
            // Only a handle can claim an error, and an orphaned one has none.
            None => no_orphan(),
        }
    }

    fn outcome<R>(&self, body: Result<R, E>, reason: Option<Shutdown>) -> Result<R, ScopeError<E>> {
        let value = self.result(body).map_err(ScopeError::Failed)?;
        match reason {
            Some(reason) => Err(reason.scope_error()),
            None => Ok(value),
        }
    }

    fn result<R>(&self, body: Result<R, E>) -> Result<R, E> {
        match (body, self.spawner.ledger.take_oldest()) {
            (Err(error), _) => Err(error),
            (Ok(_), Some(error)) => Err(error),
            (Ok(value), None) => Ok(value),
        }
    }

    /// The number of unfinished tasks, including work being dropped on abort.
    /// A concurrent [`Spawner::spawn`] call may still be registering or cleaning
    /// up work not reflected here; joining waits for those calls as well.
    pub fn len(&self) -> usize {
        self.spawner.world.check();
        self.spawner.set.lock("Tasks::unfinished").unfinished()
    }

    /// Whether every spawned task has finished, with [`len`](Self::len)'s
    /// caveat for concurrent registration.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn reap(&mut self) {
        loop {
            let next = self
                .spawner
                .set
                .lock("JoinSet::try_join_next")
                .try_join_next();
            let Some(result) = next else {
                break;
            };
            if self.spawner.ledger.reaped(result) {
                self.abort();
            }
        }
    }

    fn failed(&self) -> bool {
        self.spawner.ledger.lock().failed()
    }

    async fn resume_first_panic<T>(&mut self) -> T {
        let reaped_failure = self.spawner.ledger.lock().reaped_failure();
        if reaped_failure {
            self.abort();
            self.drain().await;
        }
        while let Some(next) = join_next(&self.spawner.set).await {
            if let Err(join_error) = next {
                self.remember_panic(join_error);
                let panicked = self.spawner.ledger.lock().panic.is_some();
                if panicked {
                    self.drain().await;
                }
            }
        }
        no_result()
    }

    // Keep the panic in the scope so it survives a cancelled drain.
    fn remember_panic(&mut self, join_error: JoinError) {
        if self.spawner.ledger.keep_panic(join_error) {
            self.abort();
        }
    }

    fn propagate_panic(&mut self) {
        let payload = self.spawner.ledger.lock().panic.take();
        if let Some(payload) = payload {
            panic::resume_unwind(payload);
        }
    }

    fn abort(&mut self) {
        self.spawner.set.lock("Tasks::abort").abort_all();
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }

    async fn drain(&mut self) {
        while let Some(next) = join_next(&self.spawner.set).await {
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
        self.spawner.world.check_unless_panicking();
        if self.spawner.world.unwinding_in_foreign_world() {
            return;
        }
        self.spawner.set.lock("Tasks::drop").abort_all();
        if let Some(cancellation) = &self.cancellation {
            cancellation.request();
        }
    }
}

impl<E: Send + 'static> Default for Tasks<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> fmt::Debug for Tasks<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.spawner.world.check();
        f.debug_struct("Tasks")
            .field("len", &self.spawner.set.lock("Tasks::fmt").len())
            .finish()
    }
}

impl<T, E> Drop for Task<T, E> {
    fn drop(&mut self) {
        if std::thread::panicking() && self.world != World::current() {
            return;
        }
        // Close before taking the receipt. Later failures orphan their own errors.
        self.rx.close();
        if let Ok(Err(_)) = self.rx.try_receive() {
            self.ledger.orphan();
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
    // Stop reading the iterator after None. It may resume or panic.
    let mut items = items.into_iter().fuse();
    let limit = limit.max(1);
    crate::runtime::in_world(World::current(), async move {
        let mut tasks = Tasks::new();
        // Tasks report here rather than through handles, so items that finish
        // behind a slow one leave only their results behind.
        let finished = locked(Vec::new());
        let mut started = 0;
        // Fill the window, then wait for a place to open. A task records its
        // error just before it ends, so this stops before taking another item.
        while !tasks.failed() {
            if tasks.spawner.set.lock("JoinSet::len").len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let finished = Arc::clone(&finished);
                tasks.spawn(async move {
                    let value = future.await?;
                    finished.lock("Tasks::record_finished").push((index, value));
                    Ok(())
                });
                continue;
            }
            match join_next(&tasks.spawner.set).await {
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
        let finished = std::mem::take(&mut *finished.lock("Tasks::take_finished"));
        if finished.len() != started {
            no_result();
        }
        let mut values = in_input_order(finished);
        values.shrink_to_fit();
        Ok(values)
    })
}

fn in_input_order<V>(mut indexed: Vec<(usize, V)>) -> Vec<V> {
    indexed.sort_unstable_by_key(|(index, _)| *index);
    indexed.into_iter().map(|(_, value)| value).collect()
}

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
        let outcomes = locked(Outcomes {
            winner: None,
            errors: Vec::new(),
        });
        let mut started = 0;
        // A task reports just before it ends, so this stops before taking
        // another item once one has succeeded.
        while outcomes.lock("Tasks::race").winner.is_none() && !tasks.failed() {
            if tasks.spawner.set.lock("JoinSet::len").len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let outcomes = Arc::clone(&outcomes);
                tasks.spawn(async move {
                    let result = future.await;
                    let mut outcomes = outcomes.lock("Tasks::race");
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
            match join_next(&tasks.spawner.set).await {
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
        let (winner, errors) = {
            let mut outcomes = outcomes.lock("Tasks::race");
            (outcomes.winner.take(), std::mem::take(&mut outcomes.errors))
        };
        if let Some(value) = winner {
            return Ok(value);
        }
        if errors.len() != started {
            no_result();
        }
        Err(in_input_order(errors))
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

    #[allow(clippy::disallowed_methods, reason = "test watchdog")]
    fn watchdog<F: Future>(future: F) -> tokio::time::Timeout<F> {
        tokio::time::timeout(Duration::from_secs(30), future)
    }

    #[cfg(feature = "deterministic")]
    fn check_and_replay<F, Fut>(seed: u64, iterations: usize, max_steps: usize, scenario: F)
    where
        F: Fn() -> Fut + Copy + Send + Sync + 'static,
        Fut: Future<Output = ()>,
    {
        let config = crate::testing::Config {
            seed,
            iterations,
            max_steps,
            ..crate::testing::Config::default()
        };
        let replay = crate::testing::check(config, scenario).unwrap();
        assert_eq!(crate::testing::replay(&replay, scenario).unwrap(), replay);
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
                    while tasks.spawner.ledger.lock().panic.is_none() {
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
        watchdog(panic_scenario()).await.unwrap();
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn reaped_panics_survive_deterministic_replay() {
        check_and_replay(187, 100, 30_000, panic_scenario);
    }

    #[tokio::test]
    async fn spawning_keeps_retained_entries_bounded_across_100_000_tasks() {
        Tasks::<()>::run(async |tasks| {
            for _ in 0..1_000 {
                let handles: Vec<_> = (0..100).map(|_| tasks.spawn(async { Ok(()) })).collect();
                assert!(
                    tasks.spawner.set.lock("JoinSet::len").len() <= 100,
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
                    tasks.spawner.set.lock("JoinSet::len").len() <= 100,
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

    #[tokio::test]
    async fn handled_spawner_reaps_completed_entries_before_join() {
        let tasks = Tasks::<()>::new();
        let spawner = tasks.spawner();
        watchdog(async {
            for _ in 0..100 {
                spawner.spawn_handled(async { Err(()) }, |()| {}).unwrap();
                while !tasks.is_empty() {
                    yield_now().await;
                }
                // One task entry and at most one coalesced completion.
                assert!(tasks.spawner.set.lock("JoinSet::len").len() <= 2);
            }
        })
        .await
        .unwrap();
        assert_eq!(tasks.join(Ok(())).await, Ok(()));
    }

    #[cfg(not(target_os = "wasi"))]
    #[tokio::test]
    async fn spawner_reaping_leaves_panics_for_scope_shutdown() {
        let tasks = Tasks::<()>::new();
        let spawner = tasks.spawner();
        spawner
            .spawn_handled(async { panic!("handled task panic") }, |(): ()| {})
            .unwrap();
        watchdog(async {
            while !tasks.is_empty() {
                yield_now().await;
            }
            spawner
                .spawn_handled(std::future::pending(), |(): ()| {})
                .unwrap();
            // The second submission reaps the first task. Joining must
            // still see its panic and abort the pending sibling.
            let panic = AssertUnwindSafe(tasks.join(Ok(())))
                .catch_unwind()
                .await
                .unwrap_err();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"handled task panic"));
        })
        .await
        .unwrap();
    }

    async fn reaped_error_during_join() {
        let mut tasks = Tasks::<&str>::new();
        let spawner = tasks.spawner();
        let set = Arc::clone(&tasks.spawner.set);
        let (release, released) = Reply::pair();
        let failed = tasks.spawn(async move {
            released.receive().await.unwrap();
            Err::<(), _>("child failed")
        });

        let mut join = pin!(tasks.join(Ok(())));
        poll_fn(|cx| {
            assert!(join.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        while set.lock("JoinSet::unfinished").unfinished() != 0 {
            yield_now().await;
        }

        let dropped = crate::Actor::new(false);
        let guard = Dropped(dropped.clone());
        spawner
            .spawn_handled(
                async move {
                    let _guard = guard;
                    std::future::pending::<Result<(), ()>>().await
                },
                |()| {},
            )
            .unwrap();
        assert_eq!(join.await, Err("child failed"));
        assert!(dropped.snapshot());
        drop(failed);
    }

    #[tokio::test]
    async fn join_observes_a_child_error_even_when_a_spawner_reaps_its_completion() {
        watchdog(reaped_error_during_join()).await.unwrap();
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn join_observes_a_reaped_completion_under_replay() {
        check_and_replay(224, 100, 10_000, reaped_error_during_join);
    }

    // Spawns connections that finish at once, then waits without spawning
    // again: only the running scope's own reaping can let go of them.
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
            while served.snapshot() < 100 || !tasks.spawner.set.lock("JoinSet::is_empty").is_empty()
            {
                crate::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_running_scope_reaps_tasks_without_another_spawn() {
        watchdog(idle_server_scenario()).await.unwrap();
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn a_running_scope_reaps_tasks_without_another_spawn_under_replay() {
        check_and_replay(195, 50, 100_000, idle_server_scenario);
    }

    // The scope's loop and the next spawn race to reap the same panic.
    // Whichever finds it, the body stops and the sibling is drained.
    #[cfg(feature = "deterministic")]
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
        check_and_replay(195, 200, 30_000, panic_reaped_by_either_scenario);
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
                    assert_eq!(tasks.spawner.set.lock("JoinSet::len").len(), 1);
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
        check_and_replay(187, 20, 100_000, scenario);
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
            while tasks.spawner.ledger.lock().recorded.len() != 1 {
                shuttle::future::yield_now().await;
            }
            let second = tasks.spawn(async { Err::<(), _>("second") });
            while tasks.spawner.ledger.lock().recorded.len() != 2 {
                shuttle::future::yield_now().await;
            }
            assert_eq!(tasks.value(first).await, Err("first"));
            let third = tasks.spawn(async { Err::<(), _>("third") });
            while tasks.spawner.ledger.lock().recorded.len() != 2 {
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
            ..Config::default()
        };
        let replay = testing::check(config, scenario).unwrap();
        assert_eq!(testing::check(config, scenario).unwrap(), replay);
        assert_eq!(testing::replay(&replay, scenario).unwrap(), replay);
    }
}
