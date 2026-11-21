use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

thread_local! {
    #[allow(clippy::disallowed_types, reason = "execution identity shared by logical tasks on this OS thread")]
    static CURRENT: Cell<Option<u64>> = const { Cell::new(None) };
}

static NEXT: AtomicU64 = AtomicU64::new(1);

#[allow(
    clippy::disallowed_methods,
    reason = "the harness runs every modeled task on the OS thread that entered it"
)]
pub(crate) fn current() -> Option<u64> {
    CURRENT.get()
}

pub(crate) struct Execution;

impl Execution {
    /// Runs one harness call on a new OS thread named like the caller's, and
    /// returns its result or resumes its panic. The call reports to the
    /// caller's `tracing` subscriber, inside the caller's current span.
    ///
    /// A failed execution can leave a task suspended partway through
    /// unwinding a panic. Shuttle leaks that task's stack, so its thread keeps
    /// reporting `std::thread::panicking()`. That thread ends with the call,
    /// and so does every thread-local the harness sets.
    #[allow(
        clippy::disallowed_methods,
        reason = "a harness call blocks its caller, which the assert below keeps out of a Tokio runtime"
    )]
    pub(crate) fn isolated<T: Send>(call: impl FnOnce() -> T + Send) -> T {
        assert!(
            current().is_none(),
            "adamantium::testing harness cannot be nested"
        );
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "adamantium::testing harness cannot run inside a Tokio runtime; use #[test], not #[tokio::test]"
        );
        let mut thread = std::thread::Builder::new();
        if let Some(name) = std::thread::current().name() {
            thread = thread.name(name.to_owned());
        }
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let span = tracing::Span::current();
        std::thread::scope(|scope| {
            #[allow(
                clippy::expect_used,
                reason = "the call can't run without its thread, as thread::spawn panics"
            )]
            let handle = thread
                .spawn_scoped(scope, move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        span.in_scope(|| {
                            let _execution = Self::enter();
                            call()
                        })
                    })
                })
                .expect("adamantium::testing harness could not start its thread");
            handle
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
        })
    }

    // The scheduler calls next even for the first execution.
    fn enter() -> Self {
        Self::claim_identity();
        Self
    }

    pub(crate) fn next(wall_epoch: SystemTime) {
        Self::claim_identity();
        super::clock::reset(wall_epoch);
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "the harness runs every modeled task on the OS thread that entered it"
    )]
    fn claim_identity() {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "adamantium::testing execution identity exhausted");
        CURRENT.set(Some(id));
    }
}

impl Drop for Execution {
    #[allow(
        clippy::disallowed_methods,
        reason = "the harness runs every modeled task on the OS thread that entered it"
    )]
    fn drop(&mut self) {
        super::clock::clear();
        CURRENT.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_runs_on_a_new_thread_named_like_the_callers() {
        let caller = std::thread::current();
        let (thread, execution) = Execution::isolated(|| (std::thread::current(), current()));
        assert_ne!(thread.id(), caller.id());
        assert!(caller.name().is_some());
        assert_eq!(thread.name(), caller.name());
        assert!(execution.is_some());
        assert_eq!(current(), None);
    }

    #[test]
    fn isolated_resumes_the_calls_panic_with_its_payload() {
        struct Payload;
        let payload = std::panic::catch_unwind(|| {
            Execution::isolated(|| std::panic::panic_any(Payload));
        })
        .unwrap_err();
        assert!(payload.is::<Payload>());
        assert_eq!(current(), None);
    }
}
