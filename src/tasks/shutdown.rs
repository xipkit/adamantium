use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

use tokio::time::Instant;

use super::{Cancellation, ScopeError};
use crate::{FeedError, ReceiveError, UntilError, time};

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

pub(super) async fn elapsed(deadline: Option<Instant>) {
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
