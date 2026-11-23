use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

use tokio::time::Instant;

use super::ScopeError;
use crate::{FeedError, ReceiveError, UntilError, Watch, runtime::World, time};

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
    pub(super) fn request(&self) {
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
