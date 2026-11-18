use std::{future::Future, pin::pin};

use std::time::Duration;

use tokio::time::Instant;

use crate::{Cancellation, ScopeError, tasks::shutdown, time};

/// How long [`retry`] waits between attempts, and how many it makes.
///
/// The first wait is the starting delay. Each later wait multiplies the last
/// by the backoff's factor, up to its largest delay. A backoff makes attempts
/// until one succeeds unless [`attempts`](Self::attempts) caps them; the
/// cancellation and deadline given to [`retry`] bound it either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub(crate) first: Duration,
    max: Duration,
    factor: u32,
    attempts: Option<u32>,
}

impl Backoff {
    /// Waits `delay` between every two attempts.
    pub fn constant(delay: Duration) -> Self {
        Self {
            first: delay,
            max: delay,
            factor: 1,
            attempts: None,
        }
    }

    /// Waits `first` after the first failure, then doubles the wait after
    /// each later one, never waiting longer than `max`.
    pub fn exponential(first: Duration, max: Duration) -> Self {
        Self {
            first: first.min(max),
            max,
            factor: 2,
            attempts: None,
        }
    }

    /// Makes at most `attempts` attempts in all, the first included. Zero is
    /// treated as one.
    #[must_use]
    pub fn attempts(self, attempts: u32) -> Self {
        Self {
            attempts: Some(attempts),
            ..self
        }
    }

    pub(crate) fn after(&self, delay: Duration) -> Duration {
        delay.saturating_mul(self.factor).min(self.max)
    }
}

/// Calls `attempt` until its future succeeds, waiting as `backoff` says after
/// each failure, until `cancellation` is requested or the absolute `deadline`
/// passes. Pass `None` for no deadline. Waits use [`crate::time`].
///
/// Returns the first success, or [`ScopeError::Failed`] with the last error
/// once the backoff's attempts run out. Shutdown ends both an attempt in
/// flight, whose future is dropped, and a wait between attempts, and returns
/// [`ScopeError::Cancelled`] or [`ScopeError::DeadlineExceeded`] without the
/// last error. As in [`Tasks::run_until`](crate::Tasks::run_until), a request
/// already present at a poll wins over a ready attempt, including before the
/// first, and cancellation wins over an equally ready deadline.
///
/// Use [`retry_if`] when some errors are not worth another attempt.
///
/// ```
/// use adamantium::{Backoff, Cancellation, retry};
/// use std::time::Duration;
/// # #[tokio::main] async fn main() {
/// let cancellation = Cancellation::new();
/// let backoff = Backoff::exponential(Duration::from_millis(1), Duration::from_millis(8));
/// let mut calls = 0;
/// let answer = retry(&cancellation, None, backoff.attempts(5), || {
///     calls += 1;
///     let result = if calls < 3 { Err("not yet") } else { Ok(42) };
///     async move { result }
/// })
/// .await;
/// assert_eq!(answer, Ok(42));
/// # }
/// ```
pub async fn retry<F, Fut, T, E>(
    cancellation: &Cancellation,
    deadline: Option<Instant>,
    backoff: Backoff,
    attempt: F,
) -> Result<T, ScopeError<E>>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send,
{
    retry_if(cancellation, deadline, backoff, attempt, |_| true).await
}

/// Like [`retry`], but makes another attempt only when `retryable` returns
/// `true` for the error. Any other error is returned at once as
/// [`ScopeError::Failed`].
pub async fn retry_if<F, Fut, T, E, P>(
    cancellation: &Cancellation,
    deadline: Option<Instant>,
    backoff: Backoff,
    mut attempt: F,
    mut retryable: P,
) -> Result<T, ScopeError<E>>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send,
    P: FnMut(&E) -> bool + Send,
{
    let mut stop = pin!(shutdown(cancellation, deadline));
    let mut delay = backoff.first;
    let mut made: u32 = 0;
    loop {
        #[expect(
            clippy::disallowed_methods,
            reason = "retry checks shutdown before starting or continuing an attempt"
        )]
        let result = tokio::select! {
            biased;
            reason = &mut stop => return Err(reason.scope_error()),
            // Calling `attempt` inside the branch means a shutdown that wins
            // this poll never calls it.
            result = async { attempt().await } => result,
        };
        made = made.saturating_add(1);
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if backoff.attempts.is_some_and(|attempts| made >= attempts) || !retryable(&error) {
            return Err(ScopeError::Failed(error));
        }
        drop(error);
        #[expect(
            clippy::disallowed_methods,
            reason = "shutdown interrupts the backoff before another attempt can start"
        )]
        let () = tokio::select! {
            biased;
            reason = &mut stop => return Err(reason.scope_error()),
            () = time::sleep(delay) => {}
        };
        delay = backoff.after(delay);
    }
}
