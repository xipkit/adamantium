use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
};

thread_local! {
    #[allow(clippy::disallowed_types, reason = "execution identity shared by logical tasks on this OS thread")]
    static CURRENT: Cell<Option<u64>> = const { Cell::new(None) };
}

static NEXT: AtomicU64 = AtomicU64::new(1);

pub(crate) fn current() -> Option<u64> {
    CURRENT.get()
}

pub(crate) struct Execution;

impl Execution {
    pub(crate) fn enter() -> Self {
        assert!(
            current().is_none(),
            "adamantium::testing harness cannot be nested"
        );
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "adamantium::testing harness cannot run inside a Tokio runtime; use #[test], not #[tokio::test]"
        );
        Self::next();
        Self
    }

    pub(crate) fn next() {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "adamantium::testing execution identity exhausted");
        CURRENT.set(Some(id));
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        CURRENT.set(None);
    }
}

/// Rejects dropping an unsupported production handle in a modeled execution.
/// Keep this as its first field so rejection precedes field destruction.
pub(crate) struct Production(pub(crate) &'static str);

impl Drop for Production {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            super::production_only(self.0);
        }
    }
}
