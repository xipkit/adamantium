//! Restarting a task that fails.

use std::{collections::VecDeque, future::Future, time::Duration};

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
    pub fn spawn_supervised<T, F, Fut>(&mut self, restarts: Restarts, mut work: F) -> Task<T, E>
    where
        T: Send + 'static,
        F: FnMut(Option<E>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawn(async move {
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
                restarted.push_back(now);
                let delay = restarts.delay(restarted.len());
                if !delay.is_zero() {
                    time::sleep(delay).await;
                }
                last = Some(error);
            }
        })
    }
}
