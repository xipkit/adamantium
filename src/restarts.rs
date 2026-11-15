//! Restarting a task that fails.

use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    task::Poll,
    time::Duration,
};

use tokio::time::Instant;

use crate::{Backoff, Task, Tasks, time};

/// How often [`Tasks::spawn_supervised`] restarts a task that fails.
///
/// A task can restart at most `limit` times in any window of length `per`.
/// Restarts older than the window no longer count, so a task that fails
/// now and then keeps running, and a task that fails again and again stops.
///
/// By default a restart is immediate. [`backoff`](Self::backoff) adds a
/// wait before each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restarts {
    limit: usize,
    per: Duration,
    backoff: Option<Backoff>,
}

impl Restarts {
    /// At most `limit` restarts in any window of length `per`. A limit of
    /// zero never restarts.
    pub fn new(limit: usize, per: Duration) -> Self {
        Self {
            limit,
            per,
            backoff: None,
        }
    }

    /// Waits before each restart as `backoff` says. The first restart in a
    /// window waits the backoff's first delay, and each further restart in
    /// the same window waits longer. Only the delays of the backoff are
    /// used; its limit on attempts is ignored.
    #[must_use]
    pub fn backoff(self, backoff: Backoff) -> Self {
        Self {
            backoff: Some(backoff),
            ..self
        }
    }

    /// The wait before a restart that is number `count` in its window,
    /// counting from one.
    fn delay(&self, count: usize) -> Duration {
        let Some(backoff) = self.backoff else {
            return Duration::ZERO;
        };
        let mut delay = backoff.first;
        for _ in 1..count {
            let next = backoff.after(delay);
            if next == delay {
                break;
            }
            delay = next;
        }
        delay
    }
}

/// Lets other tasks run once, so a task that restarts with no wait cannot
/// keep its thread and can still be aborted.
async fn yield_once() {
    let mut yielded = false;
    poll_fn(|context| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        context.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

impl<E: Send + 'static> Tasks<E> {
    /// Starts a task that is restarted when it fails.
    ///
    /// `work` makes the task's future. It is called with `None` to start the
    /// task. Each time the future returns an error, `work` is called again
    /// with that error to make a new future, so the error can be logged and
    /// the new attempt can start from clean state. A future that returns
    /// `Ok` ends the task with that value.
    ///
    /// When the task fails with no restart left under `restarts`, it ends
    /// with that error. The scope then treats it like the error of any
    /// other task. A panic is not restarted: it is resumed in the scope.
    ///
    /// The task and the waits between restarts end when the scope ends.
    ///
    /// ```
    /// use adamantium::{Restarts, Tasks};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main] async fn main() {
    /// let restarts = Restarts::new(5, Duration::from_secs(60));
    /// let served = Tasks::run(async |tasks| {
    ///     let mut starts = 0;
    ///     let worker = tasks.spawn_supervised(restarts, move |error: Option<&str>| {
    ///         if let Some(error) = error {
    ///             eprintln!("restarting after: {error}");
    ///         }
    ///         starts += 1;
    ///         let result = if starts < 3 { Err("lost connection") } else { Ok(starts) };
    ///         async move { result }
    ///     });
    ///     tasks.value(worker).await
    /// })
    /// .await;
    /// assert_eq!(served, Ok(3));
    /// # }
    /// ```
    pub fn spawn_supervised<T, F, Fut>(&mut self, restarts: Restarts, work: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnMut(Option<E>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawn(supervise(restarts, work, false))
    }

    /// Starts a named task that is restarted when it fails, with the restart
    /// contract of [`spawn_supervised`](Self::spawn_supervised).
    ///
    /// With the `tracing` feature, every start of the task runs inside one
    /// `adamantium.task` span, as for [`spawn_named`](Self::spawn_named), and
    /// the task is reported at a scope deadline until it ends for good. Each
    /// restart emits a warning in that span with `restart`, its number in
    /// the current window, and `delay`, the wait before it. Without
    /// `tracing`, this behaves as `spawn_supervised` without converting or
    /// storing the name.
    pub fn spawn_supervised_named<T, F, Fut>(
        &mut self,
        name: impl Into<crate::Str>,
        restarts: Restarts,
        work: F,
    ) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnMut(Option<E>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawn_named(name, supervise(restarts, work, true))
    }
}

/// Runs the futures that `work` makes, one after another, until one succeeds
/// or fails with no restart left. `report` says whether to emit a tracing
/// event for each restart.
async fn supervise<T, E, F, Fut>(restarts: Restarts, mut work: F, report: bool) -> Result<T, E>
where
    F: FnMut(Option<E>) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send,
    E: Send,
{
    // When each restart in the window happened, oldest first.
    let mut restarted: VecDeque<Instant> = VecDeque::new();
    let mut last = None;
    loop {
        let error = match work(last.take()).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let now = time::now();
        while restarted
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= restarts.per)
        {
            restarted.pop_front();
        }
        if restarted.len() >= restarts.limit {
            return Err(error);
        }
        let restart = restarted.len().saturating_add(1);
        let delay = restarts.delay(restart);
        #[cfg(feature = "tracing")]
        if report {
            tracing::warn!(restart, ?delay, "supervised task restarting");
        }
        #[cfg(not(feature = "tracing"))]
        let _ = report;
        if delay.is_zero() {
            yield_once().await;
        } else {
            time::sleep(delay).await;
        }
        // The window counts from when the task restarts, so a wait that
        // ends late cannot give the budget back early.
        restarted.push_back(time::now());
        last = Some(error);
    }
}
