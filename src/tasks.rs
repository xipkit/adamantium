use std::{
    fmt,
    future::Future,
    panic,
    sync::{Arc, PoisonError},
};

use tokio::{
    sync::{Semaphore, oneshot},
    task::JoinError,
};

/// The first error from a task whose handle was dropped after it finished.
#[allow(
    clippy::disallowed_types,
    reason = "a lock held only inside record and take, both synchronous"
)]
type Slot<E> = Arc<std::sync::Mutex<Option<E>>>;

/// A scope that owns the tasks spawned inside it.
///
/// No task outlives its scope: [`join`](Self::join) waits for every task,
/// and an error in any of them ends the scope with that error after the
/// others are aborted. A task's error reaches the body through
/// [`value`](Self::value); an error nobody asked for is reported when the
/// scope joins, so it is never lost. A panic in a task is resumed in the
/// scope, as a thread panic is resumed by a scoped thread. Dropping a scope
/// without joining aborts its tasks without waiting for them.
///
/// Spawning needs a Tokio runtime, as every spawn does. Use
/// [`run`](Self::run) for the common shape, or [`new`](Self::new),
/// [`spawn`](Self::spawn) and [`join`](Self::join) by hand.
pub struct Tasks<E> {
    #[allow(
        clippy::disallowed_types,
        reason = "the scope is what makes a JoinSet structured"
    )]
    set: tokio::task::JoinSet<Result<(), E>>,
    unobserved: Slot<E>,
}

/// The handle to a task spawned in a [`Tasks`] scope.
///
/// Pass it to [`Tasks::value`] to wait for the task's result. Dropping it
/// is fine: the task still runs, and its error, if any, is reported when
/// the scope joins.
pub struct Task<T, E> {
    rx: oneshot::Receiver<Result<T, E>>,
    unobserved: Slot<E>,
}

impl<T, E> Drop for Task<T, E> {
    fn drop(&mut self) {
        if let Ok(Err(error)) = self.rx.try_recv() {
            record(&self.unobserved, error);
        }
    }
}

fn record<E>(slot: &Slot<E>, error: E) {
    let mut first = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if first.is_none() {
        *first = Some(error);
    }
}

fn take<E>(slot: &Slot<E>) -> Option<E> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

impl<E: Send + 'static> Tasks<E> {
    /// An empty scope.
    #[allow(clippy::disallowed_types, reason = "see Slot")]
    pub fn new() -> Self {
        Self {
            set: tokio::task::JoinSet::new(),
            unobserved: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Runs `body` with a scope, then joins the scope. This is the usual
    /// way to use `Tasks`:
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
        let mut tasks = Self::new();
        let result = body(&mut tasks).await;
        tasks.join(result).await
    }

    /// Starts `future` as a task of this scope and returns its handle.
    pub fn spawn<T, F>(&mut self, future: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.set.spawn(async move {
            match tx.send(future.await) {
                // Delivered to the handle, or the handle was dropped and the
                // task succeeded: nothing for the scope to report.
                Ok(()) | Err(Ok(_)) => Ok(()),
                // The handle was dropped before the task failed, so the
                // scope reports the error at join.
                Err(Err(error)) => Err(error),
            }
        });
        Task {
            rx,
            unobserved: Arc::clone(&self.unobserved),
        }
    }

    /// Waits for a task and returns its result. If the task panicked, the
    /// panic is resumed here.
    pub async fn value<T: Send>(&mut self, mut task: Task<T, E>) -> Result<T, E> {
        match (&mut task.rx).await {
            Ok(result) => result,
            // A task that ends without a result has panicked, so its join
            // result is a panic. Find it.
            Err(_) => self.resume_first_panic().await,
        }
    }

    /// Waits for every task, then returns `body`'s result unless a task
    /// failed. Tasks are aborted as soon as an error is known, whether
    /// from `body` or from a task. A task's error is returned over a
    /// successful body; a failed body's error is returned over a task's.
    pub async fn join<R: Send>(mut self, body: Result<R, E>) -> Result<R, E> {
        let mut first_error = take(&self.unobserved);
        if body.is_err() || first_error.is_some() {
            self.set.abort_all();
        }
        while let Some(next) = self.set.join_next().await {
            match next {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                        self.set.abort_all();
                    }
                }
                Err(join_error) => self.resume_if_panic(join_error).await,
            }
        }
        match (body, first_error) {
            (Err(error), _) => Err(error),
            (Ok(_), Some(error)) => Err(error),
            (Ok(value), None) => Ok(value),
        }
    }

    /// The number of tasks that have not been joined.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether every spawned task has been joined.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    async fn resume_first_panic<T>(&mut self) -> T {
        while let Some(next) = self.set.join_next().await {
            match next {
                Ok(Ok(())) => {}
                Ok(Err(error)) => record(&self.unobserved, error),
                Err(join_error) => self.resume_if_panic(join_error).await,
            }
        }
        panic::resume_unwind(Box::new(
            "adamantium: a task ended without a result and without a panic",
        ))
    }

    /// Resumes a task's panic after aborting and draining the other tasks.
    /// A cancellation is this scope's own abort and is ignored.
    async fn resume_if_panic(&mut self, join_error: JoinError) {
        if let Ok(payload) = join_error.try_into_panic() {
            self.set.abort_all();
            while self.set.join_next().await.is_some() {}
            panic::resume_unwind(payload);
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
        f.debug_struct("Tasks")
            .field("len", &self.set.len())
            .finish()
    }
}

impl<T, E> fmt::Debug for Task<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Task")
    }
}

/// Runs `f` on every item, at most `limit` at a time, and returns the
/// results in input order. The first error ends the whole operation and
/// aborts the items still running. Each item runs as a task in its own
/// scope, so this needs a Tokio runtime.
pub fn concurrently<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    f: F,
) -> impl Future<Output = Result<Vec<T>, E>> + Send
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let permits = Arc::new(Semaphore::new(limit.max(1)));
    let mut tasks = Tasks::new();
    let handles: Vec<Task<T, E>> = {
        let mut f = f;
        items
            .into_iter()
            .map(|item| {
                let future = f(item);
                let permits = Arc::clone(&permits);
                tasks.spawn(async move {
                    // The permit lives until the future finishes. Acquiring
                    // fails only if the semaphore is closed, which it never is.
                    let _permit = permits.acquire_owned().await;
                    future.await
                })
            })
            .collect()
    };
    async move {
        let body = async {
            let mut values = Vec::with_capacity(handles.len());
            for handle in handles {
                values.push(tasks.value(handle).await?);
            }
            Ok(values)
        }
        .await;
        tasks.join(body).await
    }
}

/// Runs `f` on a thread meant for blocking work and returns its result. Use
/// it for code that blocks the thread it runs on, such as synchronous file
/// or network calls. A panic in `f` is resumed here.
pub async fn blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[allow(
        clippy::disallowed_methods,
        reason = "the sanctioned wrapper around spawn_blocking"
    )]
    let handle = tokio::task::spawn_blocking(f);
    match handle.await {
        Ok(value) => value,
        Err(join_error) => {
            panic::resume_unwind(join_error.try_into_panic().unwrap_or_else(|e| Box::new(e)))
        }
    }
}
