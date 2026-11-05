use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    panic,
    sync::{Arc, PoisonError},
};

use tokio::{
    sync::{Semaphore, oneshot},
    task::JoinError,
};

/// The errors of failed tasks that nobody has claimed, oldest first. A task
/// records its error here as it fails, whatever became of its handle, so the
/// scope can always find it.
struct Errors<E> {
    next: u64,
    recorded: BTreeMap<u64, E>,
}

/// What a failed task sends its handle: where to claim the error.
struct Receipt(u64);

#[allow(
    clippy::disallowed_types,
    reason = "a lock held only inside the synchronous functions below"
)]
type SharedErrors<E> = Arc<std::sync::Mutex<Errors<E>>>;

#[allow(clippy::disallowed_types, reason = "see SharedErrors")]
fn lock<E>(errors: &SharedErrors<E>) -> std::sync::MutexGuard<'_, Errors<E>> {
    errors.lock().unwrap_or_else(PoisonError::into_inner)
}

fn record<E>(errors: &SharedErrors<E>, error: E) -> Receipt {
    let mut errors = lock(errors);
    let place = errors.next;
    errors.next += 1;
    errors.recorded.insert(place, error);
    Receipt(place)
}

fn claim<E>(errors: &SharedErrors<E>, receipt: Receipt) -> Option<E> {
    lock(errors).recorded.remove(&receipt.0)
}

fn any_recorded<E>(errors: &SharedErrors<E>) -> bool {
    !lock(errors).recorded.is_empty()
}

fn take_oldest<E>(errors: &SharedErrors<E>) -> Option<E> {
    lock(errors).recorded.pop_first().map(|(_, error)| error)
}

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
    set: tokio::task::JoinSet<()>,
    errors: SharedErrors<E>,
}

/// The handle to a task spawned in a [`Tasks`] scope.
///
/// Pass it to [`Tasks::value`] on the scope that spawned the task to wait for
/// its result. Dropping it or keeping it is fine: the task still runs, and an
/// error nobody took with `value` is reported when the scope joins.
pub struct Task<T, E> {
    rx: oneshot::Receiver<Result<T, Receipt>>,
    errors: SharedErrors<E>,
}

impl<E: Send + 'static> Tasks<E> {
    /// An empty scope.
    #[allow(clippy::disallowed_types, reason = "see Slot")]
    pub fn new() -> Self {
        Self {
            set: tokio::task::JoinSet::new(),
            errors: Arc::new(std::sync::Mutex::new(Errors {
                next: 0,
                recorded: BTreeMap::new(),
            })),
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
        let errors = Arc::clone(&self.errors);
        self.set.spawn(async move {
            let result = future.await.map_err(|error| record(&errors, error));
            // Without a handle to take it, a value is dropped and an error
            // stays recorded for join.
            let _ = tx.send(result);
        });
        Task {
            rx,
            errors: Arc::clone(&self.errors),
        }
    }

    /// Waits for a task and returns its result. If the task panicked, the
    /// panic is resumed here.
    pub async fn value<T: Send>(&mut self, task: Task<T, E>) -> Result<T, E> {
        let error = match task.rx.await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(receipt)) => claim(&task.errors, receipt),
            Err(_) => None,
        };
        match error {
            Some(error) => Err(error),
            // A task that ends without a result has panicked, so its join
            // result is a panic. Find it. A result whose error is gone came
            // from another scope, whose join has already reported it.
            None => self.resume_first_panic().await,
        }
    }

    /// Waits for every task, then returns `body`'s result unless a task
    /// failed. Tasks are aborted as soon as an error is known, whether
    /// from `body` or from a task. The first error from a task, unless
    /// `body` already took it with [`value`](Self::value), is returned over
    /// a successful body; a failed body's error is returned over a task's.
    pub async fn join<R: Send>(mut self, body: Result<R, E>) -> Result<R, E> {
        let mut aborted = false;
        loop {
            // A task records its error just before it ends, so the end of
            // any task is the moment to look for a new one.
            if !aborted && (body.is_err() || any_recorded(&self.errors)) {
                self.set.abort_all();
                aborted = true;
            }
            match self.set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => self.resume_if_panic(join_error).await,
                None => break,
            }
        }
        match (body, take_oldest(&self.errors)) {
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
            if let Err(join_error) = next {
                self.resume_if_panic(join_error).await;
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
