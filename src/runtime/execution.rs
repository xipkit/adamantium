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
    /// Claims the harness for this OS thread. The scheduler starts each
    /// execution, including the first, with [`Execution::next`].
    pub(crate) fn enter() -> Self {
        assert!(
            current().is_none(),
            "adamantium::testing harness cannot be nested"
        );
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "adamantium::testing harness cannot run inside a Tokio runtime; use #[test], not #[tokio::test]"
        );
        Self::claim_identity();
        Self
    }

    /// Starts an execution with a fresh identity and a clock at its origin.
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
