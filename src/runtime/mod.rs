//! Private scheduling boundary. The production path remains available when Cargo
//! unifies the deterministic feature into an otherwise ordinary application.

#![allow(
    clippy::disallowed_types,
    reason = "the primitive implementation owns synchronization"
)]

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, PoisonError},
};

#[cfg(feature = "deterministic")]
pub(crate) mod execution;

pub(crate) mod queue;
pub(crate) mod wait;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct World {
    #[cfg(feature = "deterministic")]
    execution: Option<u64>,
}

impl World {
    pub(crate) fn current() -> Self {
        Self {
            #[cfg(feature = "deterministic")]
            execution: execution::current(),
        }
    }

    pub(crate) fn check(self) {
        assert_eq!(
            self,
            Self::current(),
            "adamantium::testing harness: primitive used outside the execution that created it"
        );
    }

    pub(crate) fn step(self) {
        self.check();
        #[cfg(feature = "deterministic")]
        if self.execution.is_some() {
            // Shuttle 0.9.10 yield_now wakes the current task, so using it
            // inside a waiter poll would turn a deadlock into a busy loop.
            // Its sleep shim only switches tasks; it does not model time.
            shuttle::thread::sleep(std::time::Duration::ZERO);
        }
    }

    pub(crate) fn step_unless_panicking(self) {
        #[cfg(feature = "deterministic")]
        if !std::thread::panicking() {
            self.step();
        }
    }
}

pub(crate) fn production_only(_primitive: &str) {
    #[cfg(feature = "deterministic")]
    assert!(
        execution::current().is_none(),
        "adamantium::testing harness: {_primitive} is not supported yet; this execution supports Actor and Channel only"
    );
}

/// Reference ownership is observable through Actor::is_shared and into_inner.
/// Yield before changing or observing it, just as before acquiring a lock.
pub(crate) struct Shared<T> {
    inner: Option<Arc<T>>,
    world: World,
}

impl<T> Shared<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: Some(Arc::new(value)),
            world: World::current(),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "only try_unwrap takes the pointer, consuming self"
    )]
    fn inner(&self) -> &Arc<T> {
        self.inner.as_ref().expect("shared pointer is present")
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.world.step();
        Arc::strong_count(self.inner()) > 1
    }

    #[allow(clippy::expect_used, reason = "self has not previously been consumed")]
    pub(crate) fn try_unwrap(mut self) -> Result<T, Self> {
        self.world.step();
        match Arc::try_unwrap(self.inner.take().expect("shared pointer is present")) {
            Ok(value) => Ok(value),
            Err(inner) => {
                self.inner = Some(inner);
                Err(self)
            }
        }
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        self.world.step();
        Self {
            inner: Some(Arc::clone(self.inner())),
            world: self.world,
        }
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.world.check();
        self.inner()
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        // A context-mismatch panic must not panic a second time in destruction.
        #[cfg(feature = "deterministic")]
        if self.inner.is_some() && !std::thread::panicking() {
            self.world.step();
        }
    }
}

pub(crate) enum Lock<T> {
    Production(std::sync::Mutex<T>),
    #[cfg(feature = "deterministic")]
    Model(shuttle::sync::Mutex<T>),
}

impl<T> Lock<T> {
    pub(crate) fn new(value: T) -> Self {
        #[cfg(feature = "deterministic")]
        if execution::current().is_some() {
            return Self::Model(shuttle::sync::Mutex::new(value));
        }
        Self::Production(std::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> Guard<'_, T> {
        match self {
            Self::Production(lock) => {
                Guard::Production(lock.lock().unwrap_or_else(PoisonError::into_inner))
            }
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => Guard::Model(lock.lock().unwrap_or_else(PoisonError::into_inner)),
        }
    }

    pub(crate) fn try_lock(&self) -> Option<Guard<'_, T>> {
        match self {
            Self::Production(lock) => lock.try_lock().ok().map(Guard::Production),
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => lock.try_lock().ok().map(Guard::Model),
        }
    }

    pub(crate) fn into_inner(self) -> T {
        match self {
            Self::Production(lock) => lock.into_inner().unwrap_or_else(PoisonError::into_inner),
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => lock.into_inner().unwrap_or_else(PoisonError::into_inner),
        }
    }
}

pub(crate) enum Guard<'a, T> {
    Production(std::sync::MutexGuard<'a, T>),
    #[cfg(feature = "deterministic")]
    Model(shuttle::sync::MutexGuard<'a, T>),
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Production(guard) => guard,
            #[cfg(feature = "deterministic")]
            Self::Model(guard) => guard,
        }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            Self::Production(guard) => guard,
            #[cfg(feature = "deterministic")]
            Self::Model(guard) => guard,
        }
    }
}

#[allow(
    clippy::future_not_send,
    reason = "Send follows the wrapped future; channel operations retain their existing bounds"
)]
pub(crate) async fn production_future<F: std::future::Future>(
    primitive: &str,
    future: F,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        production_only(primitive);
        future.as_mut().poll(cx)
    })
    .await
}
