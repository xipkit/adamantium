use std::{
    fmt,
    future::{Future, poll_fn},
    panic,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use crate::runtime::{
    Lock, World,
    task::{JoinError, JoinSet},
};
use tokio::time::Instant;

use crate::{ReplyReceiver, Watch, time};

mod batch;
mod ledger;
#[cfg(test)]
mod retention_tests;
#[cfg(all(test, feature = "deterministic"))]
mod schedule_tests;
mod shutdown;
mod spawner;
mod stream;
#[cfg(feature = "tracing")]
mod trace;
pub use batch::{concurrently, first_ok};
use ledger::{Ledger, Receipt};
use shutdown::elapsed;
pub(crate) use shutdown::{Shutdown, before_shutdown, shutdown};
pub use spawner::{SpawnError, Spawner};
use spawner::{Submitter, handled};
pub use stream::{
    concurrently_ordered, concurrently_ordered_until, concurrently_unordered,
    concurrently_unordered_until,
};

// The native lints name this type by the module that defines it, so it stays
// in this file.
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

type Locked<V> = Arc<Lock<V>>;

fn locked<V>(value: V) -> Locked<V> {
    Arc::new(Lock::new(value))
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
