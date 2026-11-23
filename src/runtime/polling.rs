// Marks the shared work this task is polling, so a call that would wait for
// that work from inside it can refuse instead of deadlocking.

use std::sync::Arc;

thread_local! {
    // Each entry lasts one poll, so task migration between polls is safe.
    static POLLING: std::cell::Cell<Vec<usize>> = const { std::cell::Cell::new(Vec::new()) };
}

#[cfg(feature = "deterministic")]
shuttle::thread_local! {
    // The list belongs to a logical task, not its OS thread.
    static MODEL_POLLING: std::cell::Cell<Vec<usize>> = const { std::cell::Cell::new(Vec::new()) };
}

#[allow(
    clippy::disallowed_methods,
    reason = "the list is changed within one poll of the work"
)]
fn with_polling<R>(f: impl FnOnce(&mut Vec<usize>) -> R) -> R {
    let apply = |cell: &std::cell::Cell<Vec<usize>>| {
        let mut owners = cell.take();
        let result = f(&mut owners);
        cell.set(owners);
        result
    };
    #[cfg(feature = "deterministic")]
    if super::execution::current().is_some() {
        return MODEL_POLLING.with(apply);
    }
    POLLING.with(apply)
}

// Entries are removed before the poll returns, while the owner is alive, so
// an address never names a freed owner.
fn address<T>(owner: &Arc<T>) -> usize {
    Arc::as_ptr(owner).addr()
}

// Whether this task is inside a poll of work that `owner` runs.
pub(crate) fn polling_here<T>(owner: &Arc<T>) -> bool {
    let owner = address(owner);
    with_polling(|owners| owners.contains(&owner))
}

// Marks work that `owner` runs as being polled until dropped.
pub(crate) struct Polling;

impl Polling {
    pub(crate) fn enter<T>(owner: &Arc<T>) -> Self {
        let owner = address(owner);
        with_polling(|owners| owners.push(owner));
        Self
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        with_polling(|owners| owners.pop());
    }
}
