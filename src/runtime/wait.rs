use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
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

    // Checks once on the fast path and again after registration to avoid a lost wake.
    pub(crate) async fn wait_for<T>(&self, mut check: impl FnMut() -> Option<T>) -> T {
        self.world
            .cooperative(async {
                loop {
                    if let Some(value) = check() {
                        return value;
                    }
                    let waiter = self.listen();
                    if let Some(value) = check() {
                        return value;
                    }
                    waiter.await;
                }
            })
            .await
    }

    pub(crate) fn listen(&self) -> Waiter {
        self.world.step_unless_panicking("WaitQueue::listen");
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
        self.world.step_unless_panicking("WaitQueue::notify_one");
        #[cfg(feature = "deterministic")]
        super::trace::record_wakeup(self.operation, self.wait_id, false);
        self.event.notify_additional(1);
    }

    pub(crate) fn notify_all(&self) {
        self.world.step_unless_panicking("WaitQueue::notify_all");
        #[cfg(feature = "deterministic")]
        super::trace::record_wakeup(self.operation, self.wait_id, true);
        self.event.notify(usize::MAX);
    }

    // Wake without a scheduling step because a waker may hold another lock.
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
        self.world.step_unless_panicking("Waiter::poll");
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
                    super::trace::end_wait(token, self.operation, self.wait_id, false);
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
        self.world.step_unless_panicking("Waiter::drop");
        #[cfg(feature = "deterministic")]
        if let Some(token) = self.trace_token.take() {
            super::trace::end_wait(token, self.operation, self.wait_id, true);
        }
        // EventListener passes an unconsumed notification to the next waiter.
    }
}

// Waiters that their owner wakes one at a time, in an order the owner keeps.
// A WaitQueue wakes whichever listener event_listener picks, so a waiter that
// may not go yet swallows the wake. The owner keeps a Turn for each waiter
// beside its own state, under its own lock, and wakes the waiter it picks.
pub(crate) struct WaitLine {
    world: World,
    #[cfg(feature = "deterministic")]
    operation: &'static str,
    #[cfg(feature = "deterministic")]
    wait_id: u64,
}

impl WaitLine {
    pub(crate) fn named(_operation: &'static str) -> Self {
        Self {
            world: World::current(),
            #[cfg(feature = "deterministic")]
            operation: _operation,
            #[cfg(feature = "deterministic")]
            wait_id: super::trace::alloc_wait_id(),
        }
    }

    pub(crate) fn world(&self) -> World {
        self.world
    }

    // Starts the wait of the task polling with `cx`, in a poll that returns
    // `Pending`. Safe under the owner's lock: it takes no step.
    pub(crate) fn wait(&self, cx: &Context<'_>) -> Turn {
        Turn {
            waker: Some(cx.waker().clone()),
            #[cfg(feature = "deterministic")]
            operation: self.operation,
            #[cfg(feature = "deterministic")]
            wait_id: self.wait_id,
            #[cfg(feature = "deterministic")]
            trace_token: super::trace::begin_turn(self.operation, self.wait_id),
        }
    }

    // Call after releasing the owner's lock: the step can switch tasks, and
    // the waker can run its task at once.
    pub(crate) fn wake(&self, wake: Wake) {
        self.world.step_unless_panicking("WaitLine::wake");
        #[cfg(feature = "deterministic")]
        if let Some(token) = wake.trace_token {
            super::trace::record_turn(token, self.operation, self.wait_id);
        }
        wake.waker.wake();
    }
}

// One waiting task. Dropping it ends the wait as cancelled.
pub(crate) struct Turn {
    // None once the owner has taken the wake.
    waker: Option<Waker>,
    #[cfg(feature = "deterministic")]
    operation: &'static str,
    #[cfg(feature = "deterministic")]
    wait_id: u64,
    #[cfg(feature = "deterministic")]
    trace_token: Option<u64>,
}

pub(crate) struct Wake {
    waker: Waker,
    #[cfg(feature = "deterministic")]
    trace_token: Option<u64>,
}

impl Turn {
    // Keeps the waker of the latest poll, and arms the turn again if its wake
    // was taken but the task still waits.
    pub(crate) fn update(&mut self, cx: &Context<'_>) {
        match &mut self.waker {
            Some(waker) => waker.clone_from(cx.waker()),
            None => self.waker = Some(cx.waker().clone()),
        }
    }

    // None if the wake was taken already and the task hasn't polled since.
    pub(crate) fn take_wake(&mut self) -> Option<Wake> {
        Some(Wake {
            waker: self.waker.take()?,
            #[cfg(feature = "deterministic")]
            trace_token: self.trace_token,
        })
    }

    // Ends the wait because the task got what it waited for.
    pub(crate) fn complete(self) {
        #[cfg(feature = "deterministic")]
        {
            let mut turn = self;
            if let Some(token) = turn.trace_token.take() {
                super::trace::end_wait(token, turn.operation, turn.wait_id, false);
            }
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        #[cfg(feature = "deterministic")]
        if let Some(token) = self.trace_token.take() {
            super::trace::end_wait(token, self.operation, self.wait_id, true);
        }
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
