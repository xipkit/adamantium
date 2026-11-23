use std::{fmt, future::Future, panic, sync::Arc};

#[cfg(doc)]
use super::Tasks;
#[cfg(feature = "tracing")]
use super::trace;
use super::{
    Locked, Task,
    ledger::{Ledger, Receipt},
    locked,
};
use crate::{
    Closed, Reply, ReplyReceiver,
    runtime::{
        World,
        task::{JoinError, JoinSet},
    },
};

#[allow(
    clippy::expect_used,
    reason = "each registered task starts at most once"
)]
fn take_work<W>(task: Locked<Option<W>>) -> W {
    task.lock("Tasks::take_work")
        .take()
        .expect("task starts once")
}

// If runtime cancellation left the caller holding the last reference, preserve
// the task's destructor panic just as the runtime would have done.
fn release_work<E, W>(ledger: &Ledger<E>, retained: Locked<Option<W>>) {
    if let Err(payload) = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(retained))) {
        ledger.keep_panic(JoinError::Panic(payload));
    }
}

// Who submits work. The owner is never refused; a handle is, once the scope
// has closed admission.
#[derive(Clone, Copy)]
pub(super) enum Submitter {
    Owner,
    Handle,
}

struct Registering<'a> {
    set: &'a Locked<JoinSet>,
    admitted: bool,
}

impl Drop for Registering<'_> {
    fn drop(&mut self) {
        if self.admitted {
            self.set.lock("JoinSet::finish_spawn").finish_spawn();
        }
    }
}

// Sends a task's result to its handle. An error the handle is no longer
// there to take stays recorded, now orphaned.
fn deliver<T, E>(ledger: &Ledger<E>, tx: Reply<Result<T, Receipt>>, result: Result<T, Receipt>) {
    if let Err(Closed(Err(_))) = tx.send(result) {
        ledger.orphan();
    }
}

pub(super) async fn handled<X>(
    future: impl Future<Output = Result<(), X>>,
    on_error: impl FnOnce(X),
) {
    if let Err(error) = future.await {
        on_error(error);
    }
}

/// A cloneable capability to add tasks to one [`Tasks`] scope.
///
/// Obtain one with [`Tasks::spawner`]. Registered children can add siblings
/// even after the body returns successfully. Idle handles do not keep the
/// scope alive: joining closes admission when the last registered task has
/// been drained. Failure shutdown and owner drop also close admission.
/// A spawn racing closure either joins the scope or returns [`SpawnError`].
///
/// Only `Tasks` can join, cancel, or claim task results with [`Tasks::value`].
/// This handle does not limit concurrency or provide sibling-to-sibling waits.
pub struct Spawner<E> {
    pub(super) set: Locked<JoinSet>,
    pub(super) world: World,
    pub(super) ledger: Arc<Ledger<E>>,
    #[cfg(feature = "tracing")]
    pub(super) named: Locked<trace::Running>,
}

/// The scope has closed admission and the submitted work was not started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnError;

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task scope is closed")
    }
}

impl std::error::Error for SpawnError {}

impl<E: Send + 'static> Spawner<E> {
    /// Starts `future` in this scope, or drops it without polling if the
    /// scope has closed admission. Dropping rejected work happens outside
    /// the admission lock, so its cleanup may safely attempt another spawn.
    ///
    /// Errors and panics follow [`Tasks::spawn`]'s rules. The returned handle
    /// can be sent to the owner for [`Tasks::value`], or dropped to leave its
    /// result to the scope. `SpawnError` is returned to the caller rather
    /// than recorded as a task failure.
    pub fn spawn<T, F>(&self, future: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.submit_future(Submitter::Handle, future)
    }

    /// Starts a named task as in [`Tasks::spawn_named`], with tracing spans
    /// and deterministic timeline names when the corresponding features are enabled.
    pub fn spawn_named<T, F>(
        &self,
        name: impl Into<crate::Str>,
        future: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.spawn(self.name_future(name, future))
    }

    /// Starts a task whose error goes to the handler, as in [`Tasks::spawn_handled`].
    /// If admission has closed, neither the future nor the handler runs.
    pub fn spawn_handled<X, F, H>(&self, future: F, on_error: H) -> Result<(), SpawnError>
    where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.submit_handled(Submitter::Handle, handled(future, on_error))
    }

    /// Starts a named task whose error goes to the handler, as in [`Tasks::spawn_handled_named`].
    pub fn spawn_handled_named<X, F, H>(
        &self,
        name: impl Into<crate::Str>,
        future: F,
        on_error: H,
    ) -> Result<(), SpawnError>
    where
        X: 'static,
        F: Future<Output = Result<(), X>> + Send + 'static,
        H: FnOnce(X) + Send + 'static,
    {
        self.submit_handled(
            Submitter::Handle,
            self.name_future(name, handled(future, on_error)),
        )
    }

    /// Registers blocking work as in [`Tasks::spawn_blocking`].
    /// Scope shutdown waits for admitted closures that have started.
    pub fn spawn_blocking<T, F>(&self, f: F) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.submit_blocking(Submitter::Handle, f)
    }

    /// Registers named blocking work as in [`Tasks::spawn_blocking_named`].
    pub fn spawn_blocking_named<T, F>(
        &self,
        name: impl Into<crate::Str>,
        f: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.spawn_blocking(self.name_blocking(name, f))
    }

    fn register<W: Send + 'static>(
        &self,
        submitter: Submitter,
        work: W,
        submit: impl FnOnce(&mut JoinSet, Locked<Option<W>>),
    ) -> Result<(), SpawnError> {
        self.world.check();
        // Keep registration alive until any synchronously cancelled work
        // has been destroyed outside the set lock.
        let mut registering = Registering {
            set: &self.set,
            admitted: false,
        };
        let retained = locked(Some(work));
        {
            let mut set = self.set.lock("Tasks::submit");
            // The owner already reaped and may still submit after observing
            // a panic, so it can drain the new work before resuming the panic.
            if matches!(submitter, Submitter::Handle) {
                if !set.begin_spawn() {
                    return Err(SpawnError);
                }
                registering.admitted = true;
                set.reap_finished();
            }
            submit(&mut set, Arc::clone(&retained));
        }
        release_work(&self.ledger, retained);
        Ok(())
    }

    pub(super) fn submit_future<T, F>(
        &self,
        submitter: Submitter,
        future: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let ledger = Arc::clone(&self.ledger);
        self.submit_handled(submitter, async move {
            let result = future.await.map_err(|error| ledger.record(error));
            deliver(&ledger, tx, result);
        })?;
        Ok(self.task(rx))
    }

    pub(super) fn submit_handled(
        &self,
        submitter: Submitter,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), SpawnError> {
        self.register(submitter, future, |set, work| {
            set.spawn(async move { take_work(work).await });
        })
    }

    pub(super) fn submit_blocking<T, F>(
        &self,
        submitter: Submitter,
        f: F,
    ) -> Result<Task<T, E>, SpawnError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        let (tx, rx) = Reply::pair();
        let ledger = Arc::clone(&self.ledger);
        self.register(
            submitter,
            move || {
                let result = f().map_err(|error| ledger.record(error));
                deliver(&ledger, tx, result);
            },
            |set, work| set.spawn_blocking(move || take_work(work)()),
        )?;
        Ok(self.task(rx))
    }

    fn task<T>(&self, rx: ReplyReceiver<Result<T, Receipt>>) -> Task<T, E> {
        Task {
            world: self.world,
            rx,
            ledger: Arc::clone(&self.ledger),
        }
    }

    pub(super) fn name_future<F: Future + Send + 'static, N: Into<crate::Str>>(
        &self,
        name: N,
        future: F,
    ) -> impl Future<Output = F::Output> + Send + 'static + use<F, N, E> {
        self.world.check();
        #[cfg(feature = "tracing")]
        let name = name.into();
        #[cfg(all(feature = "tracing", feature = "deterministic"))]
        let future = crate::runtime::trace::named(self.world, name.clone(), future);
        #[cfg(feature = "tracing")]
        let future = trace::instrument(&self.named, name, future);
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        let future = crate::runtime::trace::named(self.world, name, future);
        #[cfg(all(not(feature = "tracing"), not(feature = "deterministic")))]
        let _ = name;
        future
    }

    pub(super) fn name_blocking<T, F, N: Into<crate::Str>>(
        &self,
        name: N,
        f: F,
    ) -> impl FnOnce() -> Result<T, E> + Send + 'static + use<T, F, N, E>
    where
        T: 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        self.world.check();
        #[cfg(feature = "tracing")]
        let name = name.into();
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        let name = crate::runtime::trace::timeline_name(self.world, || name.into());
        #[cfg(feature = "tracing")]
        {
            #[cfg(feature = "deterministic")]
            let trace_name = crate::runtime::trace::timeline_name(self.world, || name.clone());
            let (running, span) = trace::track(&self.named, name);
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            move || {
                #[cfg(feature = "deterministic")]
                if let Some(trace_name) = trace_name {
                    crate::runtime::trace::record_current_task_name(&trace_name);
                }
                tracing::dispatcher::with_default(&dispatch, move || {
                    span.in_scope(move || {
                        let _running = running;
                        f()
                    })
                })
            }
        }
        #[cfg(all(not(feature = "tracing"), feature = "deterministic"))]
        {
            move || {
                if let Some(name) = name {
                    crate::runtime::trace::record_current_task_name(&name);
                }
                f()
            }
        }
        #[cfg(all(not(feature = "tracing"), not(feature = "deterministic")))]
        {
            let _ = name;
            f
        }
    }
}

impl<E> Clone for Spawner<E> {
    fn clone(&self) -> Self {
        self.world.check();
        Self {
            set: Arc::clone(&self.set),
            world: self.world,
            ledger: Arc::clone(&self.ledger),
            #[cfg(feature = "tracing")]
            named: Arc::clone(&self.named),
        }
    }
}

impl<E> fmt::Debug for Spawner<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.world.check();
        f.debug_struct("Spawner").finish_non_exhaustive()
    }
}

#[cfg(feature = "deterministic")]
impl<E> Drop for Spawner<E> {
    fn drop(&mut self) {
        self.world.check_unless_panicking();
    }
}
