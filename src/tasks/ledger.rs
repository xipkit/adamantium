use std::{any::Any, collections::BTreeMap};

use crate::runtime::{Guard, Lock, task::JoinError, wait::WaitQueue};

// Record errors even when no handle remains. Keep an observed panic until
// cleanup finishes, including when cancellation interrupts the drain.
pub(super) struct Errors<E> {
    next: u64,
    pub(super) recorded: BTreeMap<u64, E>,
    pub(super) panic: Option<Box<dyn Any + Send>>,
    orphaned: bool,
    // Whether reaping found a task cancelled that the scope never aborted.
    // Its runtime is shutting down, and its result is gone.
    reaped_cancelled: bool,
}

impl<E> Errors<E> {
    pub(super) fn must_stop(&self) -> bool {
        self.orphaned || self.panic.is_some()
    }

    pub(super) fn reaped_failure(&self) -> bool {
        self.reaped_cancelled || self.panic.is_some()
    }

    pub(super) fn failed(&self) -> bool {
        self.reaped_failure() || !self.recorded.is_empty()
    }
}

pub(super) struct Ledger<E> {
    errors: Lock<Errors<E>>,
    pub(super) on_stop: WaitQueue,
}

impl<E> Ledger<E> {
    pub(super) fn new() -> Self {
        Self {
            errors: Lock::new(Errors {
                next: 0,
                recorded: BTreeMap::new(),
                panic: None,
                orphaned: false,
                reaped_cancelled: false,
            }),
            on_stop: WaitQueue::named("tasks.stop"),
        }
    }

    pub(super) fn lock(&self) -> Guard<'_, Errors<E>> {
        self.errors.lock("Ledger::lock")
    }

    pub(super) fn record(&self, error: E) -> Receipt {
        let mut errors = self.lock();
        let place = errors.next;
        errors.next += 1;
        errors.recorded.insert(place, error);
        Receipt(place)
    }

    pub(super) fn claim(&self, receipt: Receipt) -> Option<E> {
        self.lock().recorded.remove(&receipt.0)
    }

    pub(super) fn take_oldest(&self) -> Option<E> {
        self.lock().recorded.pop_first().map(|(_, error)| error)
    }

    // Wakes the scope only for the first reason to stop, after the lock is
    // released.
    fn stop_with(&self, mark: impl FnOnce(&mut Errors<E>)) {
        let first = {
            let mut errors = self.lock();
            let first = !errors.must_stop();
            mark(&mut errors);
            first
        };
        if first {
            self.on_stop.notify_all();
        }
    }

    pub(super) fn orphan(&self) {
        self.stop_with(|errors| errors.orphaned = true);
    }

    pub(super) fn keep_panic(&self, join_error: JoinError) -> bool {
        let Ok(payload) = join_error.try_into_panic() else {
            return false;
        };
        // A payload that is not kept is dropped after the lock is released.
        let mut payload = Some(payload);
        self.stop_with(|errors| {
            if errors.panic.is_none() {
                errors.panic = payload.take();
            }
        });
        true
    }

    pub(super) fn reaped(&self, result: Result<(), JoinError>) -> bool {
        match result {
            Ok(()) => false,
            Err(JoinError::Cancelled) => {
                self.lock().reaped_cancelled = true;
                false
            }
            Err(join_error) => self.keep_panic(join_error),
        }
    }
}

pub(super) struct Receipt(u64);
