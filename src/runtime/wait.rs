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
}

impl WaitQueue {
    pub(crate) fn new() -> Self {
        Self {
            event: Event::new(),
            world: World::current(),
        }
    }

    pub(crate) fn listen(&self) -> Waiter {
        self.world.step_unless_panicking();
        Waiter {
            listener: Some(self.event.listen()),
            world: self.world,
        }
    }

    pub(crate) fn notify_one(&self) {
        self.world.step_unless_panicking();
        self.event.notify_additional(1);
    }

    pub(crate) fn notify_all(&self) {
        self.world.step_unless_panicking();
        self.event.notify(usize::MAX);
    }
}

pub(crate) struct Waiter {
    listener: Option<EventListener>,
    world: World,
}

impl Future for Waiter {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.world.step_unless_panicking();
        match &mut self.listener {
            Some(listener) => Pin::new(listener).poll(cx),
            None => Poll::Ready(()),
        }
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
                    let queue = WaitQueue::new();
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
