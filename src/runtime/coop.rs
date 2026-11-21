use super::World;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::{Context, Poll},
};

impl World {
    #[allow(clippy::future_not_send, reason = "Send follows the wrapped future")]
    pub(crate) async fn cooperative<F: Future>(self, future: F) -> F::Output {
        let mut future = pin!(future);
        poll_fn(|cx| self.poll_cooperative(cx, |cx| future.as_mut().poll(cx))).await
    }

    pub(crate) fn poll_cooperative<T>(
        self,
        cx: &mut Context<'_>,
        poll: impl FnOnce(&mut Context<'_>) -> Poll<T>,
    ) -> Poll<T> {
        self.check();
        #[cfg(feature = "deterministic")]
        if self.is_model() {
            return model::poll_cooperative(cx, poll);
        }
        #[cfg(feature = "tokio")]
        let budget = std::task::ready!(tokio::task::coop::poll_proceed(cx));
        let result = poll(cx);
        #[cfg(feature = "tokio")]
        if result.is_ready() {
            budget.made_progress();
        }
        result
    }
}

#[cfg(feature = "deterministic")]
pub(crate) mod model {
    use super::*;
    use std::cell::Cell;

    // Shuttle can switch logical tasks inside a poll, on the same OS thread.
    shuttle::thread_local! {
        static REMAINING: Cell<Option<u8>> = const { Cell::new(None) };
    }

    struct Restore<'a> {
        budget: &'a Cell<Option<u8>>,
        on_drop: Option<u8>,
    }

    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.budget.set(self.on_drop);
        }
    }

    #[allow(clippy::future_not_send, reason = "Send follows the modeled task")]
    pub(crate) async fn task<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        poll_fn(|cx| {
            REMAINING.with(|budget| {
                let _restore = Restore {
                    budget,
                    on_drop: budget.replace(Some(128)),
                };
                future.as_mut().poll(cx)
            })
        })
        .await
    }

    pub(super) fn poll_cooperative<T>(
        cx: &mut Context<'_>,
        poll: impl FnOnce(&mut Context<'_>) -> Poll<T>,
    ) -> Poll<T> {
        REMAINING.with(|budget| {
            // Futures driven outside a harness-owned task have no poll budget.
            let Some(previous) = budget.get() else {
                return poll(cx);
            };
            let Some(remaining) = previous.checked_sub(1) else {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            };
            let mut restore = Restore {
                budget,
                on_drop: budget.replace(Some(remaining)),
            };
            let result = poll(cx);
            if result.is_ready() {
                restore.on_drop = budget.get();
            }
            result
        })
    }
}
