use std::collections::BTreeMap;

use super::{Shared, Shutdown, Tasks, lock};
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

pub(super) fn track(tasks: &Shared<Running>, name: Str) -> (Registration, tracing::Span) {
    let span = tracing::info_span!("adamantium.task", task.name = %name).or_current();
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
    tasks: &Shared<Running>,
    name: Str,
    future: F,
) -> impl Future<Output = F::Output> + Send + use<F> {
    use tracing::{Instrument, instrument::WithSubscriber};

    let (running, span) = track(tasks, name);
    async move {
        let _running = running;
        future.await
    }
    .instrument(span)
    .with_current_subscriber()
}

impl<E> Tasks<E> {
    pub(super) fn report_deadline(&self, reason: &Shutdown) {
        if !matches!(reason, Shutdown::DeadlineExceeded) {
            return;
        }
        // Snapshot before cancellation and release the lock before calling a
        // subscriber, which may itself run application code.
        let running_tasks: Vec<_> = {
            let running = lock(&self.named);
            if running.next == 0 {
                return;
            }
            running.names.values().cloned().collect()
        };
        tracing::warn!(?running_tasks, "scope deadline exceeded");
    }
}
