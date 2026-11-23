#![allow(
    clippy::disallowed_types,
    reason = "the primitive implementation owns synchronization"
)]

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, PoisonError},
};

#[cfg(feature = "deterministic")]
pub(crate) mod clock;
#[cfg(feature = "deterministic")]
pub(crate) mod execution;
#[cfg(feature = "deterministic")]
pub(crate) mod trace;

pub(crate) mod coop;
pub(crate) mod polling;
pub(crate) mod queue;
#[cfg(feature = "tokio")]
pub(crate) mod task;
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

    pub(crate) fn is_model(self) -> bool {
        #[cfg(feature = "deterministic")]
        {
            self.execution.is_some()
        }
        #[cfg(not(feature = "deterministic"))]
        {
            false
        }
    }

    pub(crate) fn check(self) {
        assert_eq!(
            self,
            Self::current(),
            "adamantium::testing harness: primitive used outside the execution that created it"
        );
    }

    // A destructor that runs during an unwind must not panic again.
    pub(crate) fn check_unless_panicking(self) {
        if !std::thread::panicking() {
            self.check();
        }
    }

    // Whether a failed model execution is dropping a value that belongs to
    // another execution, which must then be left untouched.
    pub(crate) fn unwinding_in_foreign_world(self) -> bool {
        std::thread::panicking() && self.is_model() && self != Self::current()
    }

    pub(crate) fn step(self, _operation: &'static str) {
        self.check();
        #[cfg(feature = "deterministic")]
        if self.execution.is_some() {
            model_step(_operation);
        }
    }

    pub(crate) fn step_unless_panicking(self, _operation: &'static str) {
        #[cfg(feature = "deterministic")]
        if !std::thread::panicking() {
            self.step(_operation);
        }
    }
}

#[cfg(feature = "deterministic")]
#[cold]
#[inline(never)]
fn model_step(operation: &'static str) {
    let _scope = trace::enter_operation(operation);
    // Shuttle's yield_now wakes this task. Its sleep switches tasks without
    // advancing time or turning a deadlock into a busy loop.
    shuttle::thread::sleep(std::time::Duration::ZERO);
}

#[cfg(all(feature = "signal", any(unix, windows)))]
pub(crate) fn production_only(_primitive: &str) {
    #[cfg(feature = "deterministic")]
    assert!(
        execution::current().is_none(),
        "adamantium::testing harness: {_primitive} is not supported yet; this operation requires the production runtime"
    );
}

// Reference ownership is observable through Actor::is_shared and into_inner.
// Yield before changing or observing it, just as before acquiring a lock.
pub(crate) struct Shared<T> {
    inner: Option<Arc<T>>,
    world: World,
    #[cfg(feature = "deterministic")]
    drop_operation: &'static str,
}

impl<T> Shared<T> {
    pub(crate) fn new(value: T, _drop_operation: &'static str) -> Self {
        Self {
            inner: Some(Arc::new(value)),
            world: World::current(),
            #[cfg(feature = "deterministic")]
            drop_operation: _drop_operation,
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "only try_unwrap takes the pointer, consuming self"
    )]
    fn inner(&self) -> &Arc<T> {
        self.inner.as_ref().expect("shared pointer is present")
    }

    pub(crate) fn is_shared(&self, operation: &'static str) -> bool {
        self.world.step(operation);
        Arc::strong_count(self.inner()) > 1
    }

    #[allow(clippy::expect_used, reason = "self has not previously been consumed")]
    pub(crate) fn try_unwrap(mut self, operation: &'static str) -> Result<T, Self> {
        self.world.step(operation);
        match Arc::try_unwrap(self.inner.take().expect("shared pointer is present")) {
            Ok(value) => Ok(value),
            Err(inner) => {
                self.inner = Some(inner);
                Err(self)
            }
        }
    }

    pub(crate) fn clone_handle(&self, operation: &'static str) -> Self {
        self.world.step(operation);
        Self {
            inner: Some(Arc::clone(self.inner())),
            world: self.world,
            #[cfg(feature = "deterministic")]
            drop_operation: self.drop_operation,
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
            self.world.step(self.drop_operation);
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

    pub(crate) fn lock(&self, _operation: &'static str) -> Guard<'_, T> {
        match self {
            Self::Production(lock) => {
                Guard::Production(lock.lock().unwrap_or_else(PoisonError::into_inner))
            }
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => {
                let _scope = trace::enter_operation(_operation);
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                Guard::Model(ModelGuard {
                    guard: Some(guard),
                    operation: _operation,
                })
            }
        }
    }

    pub(crate) fn try_lock(&self, _operation: &'static str) -> Option<Guard<'_, T>> {
        match self {
            Self::Production(lock) => lock.try_lock().ok().map(Guard::Production),
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => {
                let _scope = trace::enter_operation(_operation);
                lock.try_lock().ok().map(|guard| {
                    Guard::Model(ModelGuard {
                        guard: Some(guard),
                        operation: _operation,
                    })
                })
            }
        }
    }

    pub(crate) fn into_inner(self, _operation: &'static str) -> T {
        match self {
            Self::Production(lock) => lock.into_inner().unwrap_or_else(PoisonError::into_inner),
            #[cfg(feature = "deterministic")]
            Self::Model(lock) => {
                let _scope = trace::enter_operation(_operation);
                lock.into_inner().unwrap_or_else(PoisonError::into_inner)
            }
        }
    }
}

pub(crate) enum Guard<'a, T> {
    Production(std::sync::MutexGuard<'a, T>),
    #[cfg(feature = "deterministic")]
    Model(ModelGuard<'a, T>),
}

#[cfg(feature = "deterministic")]
pub(crate) struct ModelGuard<'a, T> {
    guard: Option<shuttle::sync::MutexGuard<'a, T>>,
    operation: &'static str,
}

#[cfg(feature = "deterministic")]
impl<T> Drop for ModelGuard<'_, T> {
    #[cold]
    fn drop(&mut self) {
        let _scope = trace::enter_operation(self.operation);
        drop(self.guard.take());
    }
}

#[allow(clippy::expect_used, reason = "only Drop takes the modeled guard")]
impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Production(guard) => guard,
            #[cfg(feature = "deterministic")]
            Self::Model(guard) => guard.guard.as_deref().expect("modeled guard is present"),
        }
    }
}

#[allow(clippy::expect_used, reason = "only Drop takes the modeled guard")]
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            Self::Production(guard) => guard,
            #[cfg(feature = "deterministic")]
            Self::Model(guard) => guard
                .guard
                .as_deref_mut()
                .expect("modeled guard is present"),
        }
    }
}

#[cfg(feature = "tokio")]
#[allow(clippy::future_not_send, reason = "Send follows the wrapped future")]
pub(crate) async fn in_world<F: std::future::Future>(world: World, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        world.check();
        future.as_mut().poll(cx)
    })
    .await
}

#[cfg(feature = "tokio")]
pin_project_lite::pin_project! {
    pub(crate) struct InWorldStream<S> {
        world: World,
        #[pin]
        stream: S,
    }
}

#[cfg(feature = "tokio")]
pub(crate) fn in_world_stream<S>(world: World, stream: S) -> InWorldStream<S> {
    InWorldStream { world, stream }
}

#[cfg(feature = "tokio")]
impl<S: futures_core::Stream> futures_core::Stream for InWorldStream<S> {
    type Item = S::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        this.world.check();
        this.stream.poll_next(cx)
    }
}
