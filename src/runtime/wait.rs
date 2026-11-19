//! Notification boundary shared by queue and latest-value primitives.
//!
//! Register a listener, recheck the protected condition, then await it. Event
//! operations are linearization points: the harness runs on one OS thread and
//! its wakers mark tasks runnable without switching tasks inside Event's lock.
//! We schedule before each operation, including cancellation's wake handoff.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use event_listener::{Event, EventListener, Listener};

use super::World;

pub(crate) struct WaitQueue {
    event: Event,
    world: World,
    #[cfg(feature = "deterministic")]
    operation: &'static str,
    #[cfg(feature = "deterministic")]
    wait_id: u64,
}

impl WaitQueue {
    pub(crate) fn named(_operation: &'static str) -> Self {
        Self {
            event: Event::new(),
            world: World::current(),
            #[cfg(feature = "deterministic")]
            operation: _operation,
            #[cfg(feature = "deterministic")]
            wait_id: super::trace::alloc_wait_id(),
        }
    }

    pub(crate) fn listen(&self) -> Waiter {
        self.world.step_unless_panicking();
        Waiter {
            listener: Some(self.event.listen()),
            world: self.world,
            #[cfg(feature = "deterministic")]
            operation: self.operation,
            #[cfg(feature = "deterministic")]
            wait_id: self.wait_id,
            #[cfg(feature = "deterministic")]
            trace_token: super::trace::register_listener(self.wait_id),
        }
    }

    pub(crate) fn notify_one(&self) {
        self.world.step_unless_panicking();
        #[cfg(feature = "deterministic")]
        super::trace::record_wakeup(self.operation, self.wait_id, false);
        self.event.notify_additional(1);
    }

    pub(crate) fn notify_all(&self) {
        self.world.step_unless_panicking();
        #[cfg(feature = "deterministic")]
        super::trace::record_wakeup(self.operation, self.wait_id, true);
        self.event.notify(usize::MAX);
    }

    /// Notifies like [`notify_all`](Self::notify_all), but without a
    /// scheduling step, for use inside a waker. A waker can run inside
    /// another primitive's critical section, where the harness must not
    /// switch tasks.
    pub(crate) fn notify_all_from_waker(&self) {
        #[cfg(feature = "deterministic")]
        super::trace::record_wakeup(self.operation, self.wait_id, true);
        self.event.notify(usize::MAX);
    }
}

pub(crate) struct Waiter {
    listener: Option<EventListener>,
    world: World,
    #[cfg(feature = "deterministic")]
    operation: &'static str,
    #[cfg(feature = "deterministic")]
    wait_id: u64,
    #[cfg(feature = "deterministic")]
    trace_token: Option<u64>,
}

impl Future for Waiter {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.world.step_unless_panicking();
        let poll = match &mut self.listener {
            Some(listener) => Pin::new(listener).poll(cx),
            None => Poll::Ready(()),
        };
        #[cfg(feature = "deterministic")]
        match poll {
            Poll::Pending => {
                if let Some(token) = self.trace_token {
                    super::trace::begin_registered_wait(token, self.operation, self.wait_id);
                } else {
                    self.trace_token = super::trace::begin_wait(self.operation, self.wait_id);
                }
            }
            Poll::Ready(()) => {
                if let Some(token) = self.trace_token.take() {
                    super::trace::end_wait(token, self.wait_id, false);
                }
            }
        }
        poll
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if self.world != World::current() {
            // A foreign modeled waker may name a task in a finished execution.
            // Remove it without forwarding a notification before reporting misuse.
            if let Some(listener) = self.listener.take() {
                listener.discard();
            }
            if !std::thread::panicking() {
                self.world.check();
            }
            return;
        }
        self.world.step_unless_panicking();
        #[cfg(feature = "deterministic")]
        if let Some(token) = self.trace_token.take() {
            super::trace::end_wait(token, self.wait_id, true);
        }
        // EventListener passes an unconsumed notification to the next waiter.
    }
}

#[cfg(all(test, feature = "deterministic"))]
mod tests {
    use super::*;
    use crate::testing::{self, Config};
    use std::{
        future::poll_fn,
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{Arc, Mutex},
    };

    #[test]
    fn foreign_drop_discards_before_reporting_execution_mismatch() {
        let escaped = Arc::new(Mutex::new(None));
        let out = escaped.clone();
        testing::check(
            Config {
                iterations: 1,
                ..Config::default()
            },
            move || {
                let out = out.clone();
                async move {
                    let queue = WaitQueue::named("wait");
                    let mut first = queue.listen();
                    let mut second = queue.listen();
                    poll_fn(|cx| {
                        assert!(Pin::new(&mut first).poll(cx).is_pending());
                        assert!(Pin::new(&mut second).poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    queue.notify_one();
                    *out.lock().unwrap() = Some((first, second));
                }
            },
        )
        .unwrap();
        let (first, second) = escaped.lock().unwrap().take().unwrap();
        // Forwarding the first notification would invoke a stale Shuttle waker
        // during panic cleanup, aborting instead of reporting the mismatch.
        assert!(catch_unwind(AssertUnwindSafe(|| drop(first))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| drop(second))).is_err());
    }
}
