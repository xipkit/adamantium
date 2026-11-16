use std::collections::BTreeMap;

use super::{Shared, Shutdown, Tasks, lock, shared};
use crate::Str;

#[derive(Default)]
pub(super) struct Running {
    next: u64,
    names: BTreeMap<u64, Str>,
}

pub(super) struct Registration {
    tasks: Shared<Running>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.tasks).names.remove(&self.id);
    }
}

impl<E> Tasks<E> {
    pub(super) fn track(&mut self, name: Str) -> (Registration, tracing::Span) {
        let span = tracing::info_span!("adamantium.task", task.name = %name).or_current();
        let tasks = self.named.get_or_insert_with(|| shared(Running::default()));
        let id = {
            let mut running = lock(tasks);
            let id = running.next;
            running.next += 1;
            running.names.insert(id, name);
            id
        };
        let registration = Registration {
            tasks: tasks.clone(),
            id,
        };
        (registration, span)
    }

    /// Runs `future` inside its own `adamantium.task` span, under the current
    /// subscriber, and reports it as running at a deadline until it ends.
    pub(super) fn instrument<F: Future + Send>(
        &mut self,
        name: Str,
        future: F,
    ) -> impl Future<Output = F::Output> + Send + use<F, E> {
        use tracing::{Instrument, instrument::WithSubscriber};

        let (running, span) = self.track(name);
        async move {
            let _running = running;
            future.await
        }
        .instrument(span)
        .with_current_subscriber()
    }

    pub(super) fn report_deadline(&self, reason: &Shutdown) {
        if !matches!(reason, Shutdown::DeadlineExceeded) {
            return;
        }
        let Some(tasks) = &self.named else {
            return;
        };
        // Snapshot before cancellation and release the lock before calling a
        // subscriber, which may itself run application code.
        let running_tasks: Vec<_> = lock(tasks).names.values().cloned().collect();
        tracing::warn!(?running_tasks, "scope deadline exceeded");
    }
}
