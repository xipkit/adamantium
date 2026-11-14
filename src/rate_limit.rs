use std::{collections::VecDeque, future::Future, pin::pin};

use tokio::time::{Duration, Instant};

use crate::{Actor, Cancellation, ScopeError, tasks::shutdown};

/// Lets at most a number of calls start in any window of a given length.
///
/// Clones share one limit, so hand a clone to each task that should count
/// against it. A call that would go over waits for the oldest start in the
/// window to leave it. Waiting calls are not served in the order they
/// arrived. The limit remembers the start time of each call in the window,
/// so it takes memory in proportion to `permits`.
///
/// ```
/// use adamantium::{Cancellation, RateLimit, concurrently};
/// use tokio::time::Duration;
/// # async fn fetch(n: u32) -> Result<u32, std::io::Error> { Ok(n) }
/// # #[tokio::main] async fn main() -> Result<(), adamantium::ScopeError<std::io::Error>> {
/// let cancellation = Cancellation::new();
/// let limit = RateLimit::new(100, Duration::from_millis(10));
/// let pages = concurrently(0..10, 4, |n| {
///     let (limit, cancellation) = (limit.clone(), cancellation.clone());
///     async move { limit.run_until(&cancellation, None, || fetch(n)).await }
/// })
/// .await?;
/// assert_eq!(pages.len(), 10);
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct RateLimit {
    permits: usize,
    per: Duration,
    started: Actor<VecDeque<Instant>>,
}

impl RateLimit {
    /// A limit of `permits` starts in any window of length `per`. Zero
    /// permits is treated as one.
    pub fn new(permits: u32, per: Duration) -> Self {
        let permits = usize::try_from(permits.max(1)).unwrap_or(usize::MAX);
        Self {
            permits,
            per,
            started: Actor::new(VecDeque::new()),
        }
    }

    /// Waits for room under the limit, then runs the future `work` returns,
    /// until `cancellation` is requested or the absolute `deadline` passes.
    /// Pass `None` for no deadline. Waits need Tokio's time driver.
    ///
    /// The call counts against the limit from the moment `work` is called.
    /// Shutdown ends the wait for room, without taking any, or drops the
    /// work's future if it has started, and returns [`ScopeError::Cancelled`]
    /// or [`ScopeError::DeadlineExceeded`]. The work's own error comes back
    /// as [`ScopeError::Failed`]. As in [`Tasks::run_until`](crate::Tasks::run_until),
    /// a request already present at a poll wins over room under the limit and
    /// over finished work, and cancellation wins over an equally ready deadline.
    pub async fn run_until<F, Fut, T, E>(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        work: F,
    ) -> Result<T, ScopeError<E>>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, E>> + Send,
    {
        let mut stop = pin!(shutdown(cancellation, deadline));
        let mut wake: Option<Option<Instant>> = None;
        loop {
            // The first pass looks for room at once; later passes sleep until
            // the oldest start leaves the window.
            let room = async {
                match wake {
                    None => {}
                    Some(Some(wake)) => tokio::time::sleep_until(wake).await,
                    Some(None) => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                reason = &mut stop => return Err(reason.scope_error()),
                () = room => {}
            }
            match self.take() {
                Ok(()) => break,
                Err(leaves) => wake = Some(leaves),
            }
        }
        tokio::select! {
            biased;
            reason = &mut stop => Err(reason.scope_error()),
            // Calling `work` inside the branch means a shutdown that wins this
            // poll never calls it.
            result = async { work().await } => result.map_err(ScopeError::Failed),
        }
    }

    /// Records a start now if the window has room. Otherwise returns
    /// when the oldest start leaves it, or `None` if that is past the end of
    /// time.
    fn take(&self) -> Result<(), Option<Instant>> {
        self.started.with(|started| {
            // Read the clock under the lock, so starts are recorded in the
            // order they are admitted and time spent waiting for the lock
            // doesn't count against the window. Expiry looks only at the
            // front, which relies on that order.
            let now = Instant::now();
            while let Some(start) = started.front()
                && start.checked_add(self.per).is_some_and(|end| end <= now)
            {
                started.pop_front();
            }
            if started.len() < self.permits {
                started.push_back(now);
                return Ok(());
            }
            Err(started
                .front()
                .and_then(|start| start.checked_add(self.per)))
        })
    }
}
