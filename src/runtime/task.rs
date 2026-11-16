//! Runtime selection for scoped spawn/abort/join. Production stays on Tokio
//! even when the deterministic feature is unified into an application build.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "the private structured task adapter owns runtime operations"
)]

use super::World;
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

pub(crate) enum JoinError {
    Cancelled,
    Panic(Box<dyn Any + Send>),
}

impl JoinError {
    pub(crate) fn try_into_panic(self) -> Result<Box<dyn Any + Send>, Self> {
        match self {
            Self::Panic(payload) => Ok(payload),
            Self::Cancelled => Err(self),
        }
    }
}

fn production_error(error: tokio::task::JoinError) -> JoinError {
    match error.try_into_panic() {
        Ok(payload) => JoinError::Panic(payload),
        Err(_) => JoinError::Cancelled,
    }
}

pub(crate) struct JoinSet {
    inner: Set,
    world: World,
}

enum Set {
    Production {
        tasks: tokio::task::JoinSet<()>,
        unfinished: Arc<AtomicUsize>,
    },
    #[cfg(feature = "deterministic")]
    Model {
        tasks: Vec<model::Task<()>>,
        changed: std::sync::Arc<super::wait::WaitQueue>,
    },
}

struct Running(Arc<AtomicUsize>);

impl Running {
    fn new(unfinished: &Arc<AtomicUsize>) -> Self {
        unfinished.fetch_add(1, Ordering::Relaxed);
        Self(unfinished.clone())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pin_project_lite::pin_project! {
    struct Counted<F> {
        // Fields drop in declaration order: include destruction of the work
        // in the unfinished count, even on panic or abort before first poll.
        #[pin]
        work: F,
        _running: Running,
    }
}

impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().work.poll(cx)
    }
}

#[cfg(not(target_os = "wasi"))]
impl<F: FnOnce()> Counted<F> {
    fn run(self) {
        (self.work)();
    }
}

impl JoinSet {
    pub(crate) fn new() -> Self {
        let world = World::current();
        #[cfg(feature = "deterministic")]
        if super::execution::current().is_some() {
            return Self {
                world,
                inner: Set::Model {
                    tasks: Vec::new(),
                    changed: std::sync::Arc::new(super::wait::WaitQueue::new()),
                },
            };
        }
        Self {
            world,
            inner: Set::Production {
                tasks: tokio::task::JoinSet::new(),
                unfinished: Arc::new(AtomicUsize::new(0)),
            },
        }
    }

    pub(crate) fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.world.step();
        match &mut self.inner {
            Set::Production { tasks, unfinished } => {
                tasks.spawn(Counted {
                    work: future,
                    _running: Running::new(unfinished),
                });
            }
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, changed } => tasks.push(model::spawn(future, changed.clone())),
        }
    }

    pub(crate) fn spawn_blocking(&mut self, f: impl FnOnce() + Send + 'static) {
        self.world.step();
        match &mut self.inner {
            // WASI has one thread, so the closure runs inside a task of its own.
            #[cfg(target_os = "wasi")]
            Set::Production { tasks, unfinished } => {
                tasks.spawn(Counted {
                    work: async move { f() },
                    _running: Running::new(unfinished),
                });
            }
            #[cfg(not(target_os = "wasi"))]
            Set::Production { tasks, unfinished } => {
                let counted = Counted {
                    work: f,
                    _running: Running::new(unfinished),
                };
                tasks.spawn_blocking(move || counted.run());
            }
            // Queue before entering the closure, as a blocking pool does. Once
            // entered, f runs synchronously to completion even if a modeled
            // lock switches tasks inside it; abort cannot interrupt that poll.
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, changed } => tasks.push(model::spawn(
                async move {
                    shuttle::future::yield_now().await;
                    f()
                },
                changed.clone(),
            )),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.world.check();
        match &self.inner {
            Set::Production { tasks, .. } => tasks.len(),
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, .. } => tasks.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn unfinished(&self) -> usize {
        self.world.step();
        match &self.inner {
            Set::Production { unfinished, .. } => unfinished.load(Ordering::Relaxed),
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, .. } => tasks.iter().filter(|task| !task.is_finished()).count(),
        }
    }

    pub(crate) fn abort_all(&mut self) {
        self.world.step_unless_panicking();
        match &mut self.inner {
            Set::Production { tasks, .. } => tasks.abort_all(),
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, .. } => {
                for task in tasks {
                    task.abort();
                }
            }
        }
    }

    pub(crate) fn try_join_next(&mut self) -> Option<Result<(), JoinError>> {
        self.world.step();
        match &mut self.inner {
            Set::Production { tasks, .. } => {
                tasks.try_join_next().map(|r| r.map_err(production_error))
            }
            #[cfg(feature = "deterministic")]
            Set::Model { tasks, .. } => {
                for (index, task) in tasks.iter().enumerate() {
                    if let Some(result) = task.take() {
                        drop(tasks.swap_remove(index));
                        return Some(result);
                    }
                }
                None
            }
        }
    }

    pub(crate) async fn join_next(&mut self) -> Option<Result<(), JoinError>> {
        super::in_world(self.world, async {
            match &mut self.inner {
                Set::Production { tasks, .. } => {
                    tasks.join_next().await.map(|r| r.map_err(production_error))
                }
                #[cfg(feature = "deterministic")]
                Set::Model { tasks, changed } => loop {
                    if tasks.is_empty() {
                        return None;
                    }
                    let listener = changed.listen();
                    for (index, task) in tasks.iter().enumerate() {
                        if let Some(result) = task.take() {
                            tasks.remove(index);
                            return Some(result);
                        }
                    }
                    listener.await;
                },
            }
        })
        .await
    }
}

impl Drop for JoinSet {
    fn drop(&mut self) {
        if self.world != World::current() {
            if !std::thread::panicking() {
                self.world.check();
            }
            return;
        }
        self.abort_all();
    }
}

pub(crate) enum Blocking<T> {
    Production(tokio::task::JoinHandle<T>),
    #[cfg(feature = "deterministic")]
    Model(model::Task<T>),
}

impl<T> Future for Blocking<T> {
    type Output = Result<T, JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            Self::Production(handle) => Pin::new(handle)
                .poll(cx)
                .map(|r| r.map_err(production_error)),
            #[cfg(feature = "deterministic")]
            Self::Model(task) => Pin::new(task).poll(cx),
        }
    }
}

pub(crate) fn spawn_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Blocking<T> {
    #[cfg(feature = "deterministic")]
    if super::execution::current().is_some() {
        return Blocking::Model(model::spawn(
            async move {
                shuttle::future::yield_now().await;
                f()
            },
            std::sync::Arc::new(super::wait::WaitQueue::new()),
        ));
    }
    // WASI has one thread, so the closure runs inside a task of its own.
    #[cfg(target_os = "wasi")]
    return Blocking::Production(tokio::task::spawn(async move { f() }));
    #[cfg(not(target_os = "wasi"))]
    Blocking::Production(tokio::task::spawn_blocking(f))
}

#[cfg(feature = "deterministic")]
mod model {
    use super::*;
    use crate::runtime::{
        Lock,
        wait::{WaitQueue, Waiter},
    };
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc,
    };

    struct Completion<T> {
        result: Lock<Option<Result<T, JoinError>>>,
        changed: Arc<WaitQueue>,
    }

    pub(crate) struct Task<T> {
        abort: shuttle::future::AbortHandle,
        completion: Arc<Completion<T>>,
        waiter: Option<Waiter>,
        world: World,
    }

    impl<T> Task<T> {
        pub(super) fn is_finished(&self) -> bool {
            self.completion.result.lock().is_some()
        }

        pub(super) fn abort(&self) {
            self.abort.abort();
        }
        pub(super) fn take(&self) -> Option<Result<T, JoinError>> {
            self.completion.result.lock().take()
        }
    }

    impl<T> Future for Task<T> {
        type Output = Result<T, JoinError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.world.check();
            loop {
                if self.waiter.is_none() {
                    self.waiter = Some(self.completion.changed.listen());
                }
                if let Some(result) = self.take() {
                    return Poll::Ready(result);
                }
                if let Some(waiter) = &mut self.waiter
                    && Pin::new(waiter).poll(cx).is_pending()
                {
                    return Poll::Pending;
                }
                self.waiter = None;
            }
        }
    }

    struct Transport<F: Future> {
        future: Option<Pin<Box<F>>>,
        completion: Arc<Completion<F::Output>>,
    }

    impl<F: Future> Transport<F> {
        fn finish(&mut self, result: Result<F::Output, JoinError>) {
            // Destruction is part of the task, including abort before first
            // poll. Publish only after both polling and destruction are caught.
            let destroyed = catch_unwind(AssertUnwindSafe(|| drop(self.future.take())));
            let result = match (result, destroyed) {
                (Err(JoinError::Panic(payload)), _) => Err(JoinError::Panic(payload)),
                (_, Err(payload)) => Err(JoinError::Panic(payload)),
                (result, Ok(())) => result,
            };
            *self.completion.result.lock() = Some(result);
            self.completion.changed.notify_all();
        }
    }

    impl<F: Future> Future for Transport<F> {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let polled = catch_unwind(AssertUnwindSafe(|| match &mut self.future {
                Some(future) => future.as_mut().poll(cx),
                None => Poll::Pending,
            }));
            let result = match polled {
                Ok(Poll::Pending) => return Poll::Pending,
                Ok(Poll::Ready(value)) => Ok(value),
                Err(payload) => Err(JoinError::Panic(payload)),
            };
            self.finish(result);
            Poll::Ready(())
        }
    }

    impl<F: Future> Drop for Transport<F> {
        fn drop(&mut self) {
            if self.future.is_some() {
                self.finish(Err(JoinError::Cancelled));
            }
        }
    }

    pub(super) fn spawn<F>(future: F, changed: Arc<WaitQueue>) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let completion = Arc::new(Completion {
            result: Lock::new(None),
            changed,
        });
        let handle = shuttle::future::spawn(Transport {
            future: Some(Box::pin(future)),
            completion: completion.clone(),
        });
        let abort = handle.abort_handle();
        // Shuttle's JoinHandle destructor accesses the current execution by
        // task ID. Keep it inside this execution; the owned abort permission
        // and our completion slot are safe to destroy after a misuse rejection.
        drop(handle);
        Task {
            abort,
            completion,
            waiter: None,
            world: World::current(),
        }
    }
}
