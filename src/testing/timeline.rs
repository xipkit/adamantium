use std::{collections::BTreeMap, fmt, time::Duration};

#[cfg(doc)]
use super::explain;
use super::{FailureKind, Replay};
use crate::runtime::trace::{Collector, RawEvent, RawKind, Scheduling};

/// A task observed during [`explain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInfo {
    id: usize,
    name: Option<String>,
}

impl TaskInfo {
    /// The logical task identifier in the execution.
    pub fn id(&self) -> usize {
        self.id
    }

    /// The task's name, if it was spawned with a name or provided by the runtime.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// A task still waiting on an operation when the execution ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitingTask {
    task_id: usize,
    task_name: Option<String>,
    operation: &'static str,
    wait_id: u64,
    since: Duration,
}

impl WaitingTask {
    /// The waiting task's identifier.
    pub fn task_id(&self) -> usize {
        self.task_id
    }

    /// The waiting task's name, if known.
    pub fn task_name(&self) -> Option<&str> {
        self.task_name.as_deref()
    }

    /// The operation the task is waiting on (for example `channel.receive` or `time.sleep`).
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The wait instance identifier within the execution.
    pub fn wait_id(&self) -> u64 {
        self.wait_id
    }

    /// The virtual timestamp when the task entered this wait.
    pub fn since(&self) -> Duration {
        self.since
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineEventKind {
    Scheduled {
        task_id: usize,
        task_name: Option<String>,
    },
    Wait {
        task_id: usize,
        task_name: Option<String>,
        operation: &'static str,
        wait_id: u64,
    },
    Wakeup {
        by_task_id: Option<usize>,
        by_task_name: Option<String>,
        target_task_ids: Vec<usize>,
        target_task_names: Vec<Option<String>>,
        operation: &'static str,
        wait_id: u64,
    },
    WaitCompleted {
        task_id: usize,
        task_name: Option<String>,
        operation: &'static str,
        wait_id: u64,
    },
    WaitCancelled {
        task_id: usize,
        task_name: Option<String>,
        operation: &'static str,
        wait_id: u64,
    },
    Failed {
        kind: FailureKind,
        task_id: Option<usize>,
        task_name: Option<String>,
    },
}

/// A timestamped entry in a reconstructed [`Timeline`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineEvent {
    timestamp: Duration,
    kind: TimelineEventKind,
    scheduling: Option<Scheduling>,
}

impl TimelineEvent {
    /// The virtual timestamp elapsed since the start of the execution.
    pub fn timestamp(&self) -> Duration {
        self.timestamp
    }

    /// The event details.
    pub fn kind(&self) -> &TimelineEventKind {
        &self.kind
    }

    /// The modeled operation about to resume, if this is a labeled scheduling event.
    pub fn scheduled_operation(&self) -> Option<&'static str> {
        self.scheduling.as_ref()?.operation
    }

    /// The number of consecutive scheduling decisions represented by this event.
    /// Returns `None` for other kinds of events.
    pub fn scheduled_steps(&self) -> Option<usize> {
        self.scheduling.as_ref().map(|scheduling| scheduling.steps)
    }
}

/// Payload-free named task timeline reconstructed from a [`Replay`].
#[derive(Clone, PartialEq, Eq)]
pub struct Timeline {
    failure: Option<FailureKind>,
    pub(super) replay: Replay,
    tasks: Vec<TaskInfo>,
    waiting: Vec<WaitingTask>,
    pub(super) events: Vec<TimelineEvent>,
}

impl Timeline {
    /// The failure category, or `None` if the replayed execution passed.
    pub fn failure(&self) -> Option<FailureKind> {
        self.failure
    }

    /// The observed execution record from the traced replay.
    pub fn replay(&self) -> &Replay {
        &self.replay
    }

    /// Tasks observed during the execution, ordered by task ID.
    pub fn tasks(&self) -> &[TaskInfo] {
        &self.tasks
    }

    /// Tasks still waiting on an operation when the execution ended.
    pub fn waiting(&self) -> &[WaitingTask] {
        &self.waiting
    }

    /// Reconstructed timeline events in execution order.
    pub fn events(&self) -> &[TimelineEvent] {
        &self.events
    }
}

fn format_task_label(f: &mut fmt::Formatter<'_>, id: usize, name: Option<&str>) -> fmt::Result {
    match name {
        Some(name) => write!(f, "task#{id} {name:?}"),
        None => write!(f, "task#{id}"),
    }
}

impl fmt::Debug for Timeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Timeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure {
            Some(kind) => write!(f, "adamantium::testing {kind:?}")?,
            None => write!(f, "adamantium::testing Passed")?,
        }
        writeln!(
            f,
            " (scheduler={:?}, seed={}, iteration={}, max_steps={}):",
            self.replay.scheduler, self.replay.seed, self.replay.iteration, self.replay.max_steps
        )?;
        self.fmt_events(f)
    }
}

impl Timeline {
    pub(super) fn fmt_events(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.waiting.is_empty() {
            writeln!(f, "Waiting tasks:")?;
            for entry in &self.waiting {
                write!(f, "  - ")?;
                format_task_label(f, entry.task_id, entry.task_name.as_deref())?;
                writeln!(
                    f,
                    " waiting on {} (#{}) since {:?}",
                    entry.operation, entry.wait_id, entry.since
                )?;
            }
        }
        writeln!(f, "Timeline:")?;
        for event in &self.events {
            write!(f, "  {:?} ", event.timestamp)?;
            match &event.kind {
                TimelineEventKind::Scheduled { task_id, task_name } => {
                    format_task_label(f, *task_id, task_name.as_deref())?;
                    write!(f, " {}", event.scheduled_operation().unwrap_or("scheduled"))?;
                    if let Some(steps) = event.scheduled_steps().filter(|&steps| steps > 1) {
                        write!(f, " (×{steps})")?;
                    }
                    writeln!(f)?;
                }
                TimelineEventKind::Wait {
                    task_id,
                    task_name,
                    operation,
                    wait_id,
                } => {
                    format_task_label(f, *task_id, task_name.as_deref())?;
                    writeln!(f, " waiting on {operation} (#{wait_id})")?;
                }
                TimelineEventKind::Wakeup {
                    by_task_id,
                    by_task_name,
                    target_task_ids,
                    target_task_names,
                    operation,
                    wait_id,
                } => {
                    if let Some(by_id) = by_task_id {
                        format_task_label(f, *by_id, by_task_name.as_deref())?;
                        write!(f, " ")?;
                    }
                    write!(f, "woke ")?;
                    for (index, target_id) in target_task_ids.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        let target_name = target_task_names
                            .get(index)
                            .and_then(|name| name.as_deref());
                        format_task_label(f, *target_id, target_name)?;
                    }
                    writeln!(f, " from {operation} (#{wait_id})")?;
                }
                TimelineEventKind::WaitCompleted {
                    task_id,
                    task_name,
                    operation,
                    wait_id,
                } => {
                    format_task_label(f, *task_id, task_name.as_deref())?;
                    writeln!(f, " resumed from {operation} (#{wait_id})")?;
                }
                TimelineEventKind::WaitCancelled {
                    task_id,
                    task_name,
                    operation,
                    wait_id,
                } => {
                    format_task_label(f, *task_id, task_name.as_deref())?;
                    writeln!(f, " cancelled wait on {operation} (#{wait_id})")?;
                }
                TimelineEventKind::Failed {
                    kind,
                    task_id,
                    task_name,
                } => {
                    write!(f, "failed ({kind:?})")?;
                    if let Some(id) = task_id {
                        write!(f, " at ")?;
                        format_task_label(f, *id, task_name.as_deref())?;
                    }
                    writeln!(f)?;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn build_timeline(
    observed_replay: Replay,
    failure: Option<FailureKind>,
    collector: Option<Collector>,
) -> Timeline {
    let Some(collector) = collector else {
        return Timeline {
            failure,
            replay: observed_replay,
            tasks: Vec::new(),
            waiting: Vec::new(),
            events: Vec::new(),
        };
    };

    let names = collector.names;
    let mut seen: BTreeMap<usize, Option<String>> = names
        .iter()
        .map(|(id, name)| (*id, Some(name.clone())))
        .collect();

    let mut events = Vec::with_capacity(collector.events.len() + usize::from(failure.is_some()));
    let mut last_timestamp = Duration::ZERO;
    for RawEvent { timestamp, kind } in collector.events {
        let (kind, scheduling) = resolve_event(kind, &names, &mut seen);
        last_timestamp = timestamp;
        events.push(TimelineEvent {
            timestamp,
            kind,
            scheduling,
        });
    }

    if let Some(kind) = failure {
        let task_id = collector.last_scheduled;
        events.push(TimelineEvent {
            timestamp: last_timestamp,
            kind: TimelineEventKind::Failed {
                kind,
                task_id,
                task_name: task_id.and_then(|id| see_task(&names, &mut seen, id)),
            },
            scheduling: None,
        });
    }

    let tasks = seen
        .into_iter()
        .map(|(id, name)| TaskInfo { id, name })
        .collect();

    let waiting_entries = if failure.is_some() {
        collector
            .teardown_snapshot
            .unwrap_or_else(|| collector.active_waits.into_values().collect())
    } else {
        collector.active_waits.into_values().collect()
    };

    let waiting = waiting_entries
        .into_iter()
        .map(|entry| WaitingTask {
            task_id: entry.task_id,
            task_name: names.get(&entry.task_id).cloned(),
            operation: entry.operation,
            wait_id: entry.wait_id,
            since: entry.since,
        })
        .collect();

    Timeline {
        failure,
        replay: observed_replay,
        tasks,
        waiting,
        events,
    }
}

// Lists the task among those the timeline saw and returns its name.
fn see_task(
    names: &BTreeMap<usize, String>,
    seen: &mut BTreeMap<usize, Option<String>>,
    id: usize,
) -> Option<String> {
    let name = names.get(&id).cloned();
    seen.entry(id).or_insert_with(|| name.clone());
    name
}

fn resolve_event(
    kind: RawKind,
    names: &BTreeMap<usize, String>,
    seen: &mut BTreeMap<usize, Option<String>>,
) -> (TimelineEventKind, Option<Scheduling>) {
    let mut see = |id| see_task(names, seen, id);
    match kind {
        RawKind::Scheduled {
            task_id,
            scheduling,
        } => (
            TimelineEventKind::Scheduled {
                task_id,
                task_name: see(task_id),
            },
            Some(scheduling),
        ),
        RawKind::Wait {
            task_id,
            operation,
            wait_id,
        } => (
            TimelineEventKind::Wait {
                task_id,
                task_name: see(task_id),
                operation,
                wait_id,
            },
            None,
        ),
        RawKind::Wakeup {
            by_task_id,
            target_task_ids,
            operation,
            wait_id,
        } => {
            let by_task_name = by_task_id.and_then(&mut see);
            let target_task_names = target_task_ids.iter().map(|id| see(*id)).collect();
            (
                TimelineEventKind::Wakeup {
                    by_task_id,
                    by_task_name,
                    target_task_ids,
                    target_task_names,
                    operation,
                    wait_id,
                },
                None,
            )
        }
        RawKind::WaitCompleted {
            task_id,
            operation,
            wait_id,
        } => (
            TimelineEventKind::WaitCompleted {
                task_id,
                task_name: see(task_id),
                operation,
                wait_id,
            },
            None,
        ),
        RawKind::WaitCancelled {
            task_id,
            operation,
            wait_id,
        } => (
            TimelineEventKind::WaitCancelled {
                task_id,
                task_name: see(task_id),
                operation,
                wait_id,
            },
            None,
        ),
    }
}
