//! Timeline collector for the deterministic testing harness.
//!
//! Records task names, virtual timestamps, wait operations and wakeups without
//! calling into Shuttle or altering scheduling decisions.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "timeline tracing is confined to the harness OS thread"
)]

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use shuttle_engine::runtime::execution::ExecutionState;

use super::clock;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static COLLECTOR: RefCell<Option<Collector>> = const { RefCell::new(None) };
}

pub(crate) fn is_active() -> bool {
    ACTIVE.get()
}

pub(crate) fn is_teardown() -> bool {
    std::thread::panicking()
        || ExecutionState::try_with(|state| state.in_cleanup()).unwrap_or(false)
}

#[derive(Clone)]
pub(crate) struct WaitEntry {
    pub(crate) task_id: usize,
    pub(crate) operation: &'static str,
    pub(crate) wait_id: u64,
    pub(crate) since: Duration,
}

struct QueueWaiter {
    token: u64,
    task_id: usize,
    notified: bool,
}

pub(crate) enum RawEvent {
    Scheduled {
        timestamp: Duration,
        task_id: usize,
    },
    Wait {
        timestamp: Duration,
        task_id: usize,
        operation: &'static str,
        wait_id: u64,
    },
    Wakeup {
        timestamp: Duration,
        by_task_id: Option<usize>,
        target_task_ids: Vec<usize>,
        operation: &'static str,
        wait_id: u64,
    },
    WaitCompleted {
        timestamp: Duration,
        task_id: usize,
        operation: &'static str,
        wait_id: u64,
    },
    WaitCancelled {
        timestamp: Duration,
        task_id: usize,
        operation: &'static str,
        wait_id: u64,
    },
}

pub(crate) struct Collector {
    next_id: u64,
    pub(crate) names: BTreeMap<usize, String>,
    pub(crate) active_waits: BTreeMap<u64, WaitEntry>,
    pub(crate) teardown_snapshot: Option<Vec<WaitEntry>>,
    queue_waiters: BTreeMap<u64, Vec<QueueWaiter>>,
    pub(crate) events: Vec<RawEvent>,
    pub(crate) last_scheduled: Option<usize>,
}

impl Collector {
    fn new() -> Self {
        let mut names = BTreeMap::new();
        names.insert(0, "root".to_owned());
        Self {
            next_id: 1,
            names,
            active_waits: BTreeMap::new(),
            teardown_snapshot: None,
            queue_waiters: BTreeMap::new(),
            events: Vec::new(),
            last_scheduled: None,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

pub(crate) struct Guard {
    _private: (),
}

impl Guard {
    pub(crate) fn enter() -> Self {
        ACTIVE.set(true);
        Self { _private: () }
    }

    pub(crate) fn reset() {
        COLLECTOR.set(Some(Collector::new()));
    }

    pub(crate) fn finish(self) -> Option<Collector> {
        ACTIVE.set(false);
        COLLECTOR.take()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.set(false);
        COLLECTOR.set(None);
    }
}

fn with_collector<R>(f: impl FnOnce(&mut Collector) -> R) -> Option<R> {
    if !ACTIVE.get() {
        return None;
    }
    COLLECTOR.with_borrow_mut(|slot| slot.as_mut().map(f))
}

fn current_task_id() -> Option<usize> {
    ExecutionState::try_with(|state| state.try_current().map(|task| usize::from(task.id())))
        .ok()
        .flatten()
}

pub(crate) fn alloc_wait_id() -> u64 {
    with_collector(Collector::alloc_id).unwrap_or(0)
}

pub(crate) fn record_task_name(task_id: usize, name: &str) {
    with_collector(|collector| {
        collector
            .names
            .entry(task_id)
            .or_insert_with(|| name.to_owned());
    });
}

pub(crate) fn record_current_task_name(name: &str) {
    if is_active()
        && let Some(task_id) = current_task_id()
    {
        record_task_name(task_id, name);
    }
}

pub(crate) fn record_scheduled(task_id: usize, shuttle_name: Option<String>) {
    with_collector(|collector| {
        if !is_teardown() {
            collector.teardown_snapshot = None;
        }
        if let Some(name) = shuttle_name {
            collector.names.entry(task_id).or_insert(name);
        }
        if collector.last_scheduled != Some(task_id) {
            collector.last_scheduled = Some(task_id);
            collector.events.push(RawEvent::Scheduled {
                timestamp: clock::elapsed(),
                task_id,
            });
        }
    });
}

pub(crate) fn register_listener(wait_id: u64) -> Option<u64> {
    if !is_active() || wait_id == 0 {
        return None;
    }
    let task_id = current_task_id()?;
    with_collector(|collector| {
        if !is_teardown() {
            collector.teardown_snapshot = None;
        }
        let token = collector.alloc_id();
        collector
            .queue_waiters
            .entry(wait_id)
            .or_default()
            .push(QueueWaiter {
                token,
                task_id,
                notified: false,
            });
        token
    })
}

pub(crate) fn begin_registered_wait(token: u64, operation: &'static str, wait_id: u64) {
    if !is_active() || wait_id == 0 {
        return;
    }
    let Some(task_id) = current_task_id() else {
        return;
    };
    with_collector(|collector| {
        if !is_teardown() {
            collector.teardown_snapshot = None;
        }
        if collector.active_waits.contains_key(&token) {
            return;
        }
        let timestamp = clock::elapsed();
        collector.active_waits.insert(
            token,
            WaitEntry {
                task_id,
                operation,
                wait_id,
                since: timestamp,
            },
        );
        collector.events.push(RawEvent::Wait {
            timestamp,
            task_id,
            operation,
            wait_id,
        });
    });
}

pub(crate) fn begin_wait(operation: &'static str, wait_id: u64) -> Option<u64> {
    let token = register_listener(wait_id)?;
    begin_registered_wait(token, operation, wait_id);
    Some(token)
}

pub(crate) fn end_wait(token: u64, operation: &'static str, wait_id: u64, cancelled: bool) {
    let teardown = cancelled && is_teardown();
    let by_task_id = current_task_id();
    with_collector(|collector| {
        if teardown {
            if collector.teardown_snapshot.is_none() {
                collector.teardown_snapshot =
                    Some(collector.active_waits.values().cloned().collect());
            }
        } else {
            collector.teardown_snapshot = None;
        }
        let entry = collector.active_waits.remove(&token);
        let queue_id = entry.as_ref().map_or(wait_id, |entry| entry.wait_id);
        let op = entry.as_ref().map_or(operation, |entry| entry.operation);
        let mut handed_off = None;
        if let Some(waiters) = collector.queue_waiters.get_mut(&queue_id) {
            let mut was_notified = false;
            waiters.retain(|waiter| {
                if waiter.token == token {
                    was_notified = waiter.notified;
                    false
                } else {
                    true
                }
            });
            if cancelled
                && was_notified
                && let Some(next) = waiters.iter_mut().find(|waiter| !waiter.notified)
            {
                next.notified = true;
                handed_off = Some(next.task_id);
            }
            if waiters.is_empty() {
                collector.queue_waiters.remove(&queue_id);
            }
        }
        let timestamp = clock::elapsed();
        if let Some(entry) = entry {
            let event = if cancelled {
                RawEvent::WaitCancelled {
                    timestamp,
                    task_id: entry.task_id,
                    operation: entry.operation,
                    wait_id: entry.wait_id,
                }
            } else {
                RawEvent::WaitCompleted {
                    timestamp,
                    task_id: entry.task_id,
                    operation: entry.operation,
                    wait_id: entry.wait_id,
                }
            };
            collector.events.push(event);
        }
        if let Some(target_task_id) = handed_off {
            collector.events.push(RawEvent::Wakeup {
                timestamp,
                by_task_id,
                target_task_ids: vec![target_task_id],
                operation: op,
                wait_id: queue_id,
            });
        }
    });
}

pub(crate) fn record_wakeup(operation: &'static str, wait_id: u64, all: bool) {
    if !is_active() || wait_id == 0 {
        return;
    }
    let by_task_id = current_task_id();
    with_collector(|collector| {
        if !is_teardown() {
            collector.teardown_snapshot = None;
        }
        let Some(waiters) = collector.queue_waiters.get_mut(&wait_id) else {
            return;
        };
        let mut target_task_ids = Vec::new();
        if all {
            for waiter in waiters.iter_mut() {
                if !waiter.notified {
                    waiter.notified = true;
                    target_task_ids.push(waiter.task_id);
                }
            }
        } else if let Some(waiter) = waiters.iter_mut().find(|waiter| !waiter.notified) {
            waiter.notified = true;
            target_task_ids.push(waiter.task_id);
        }
        if target_task_ids.is_empty() {
            return;
        }
        collector.events.push(RawEvent::Wakeup {
            timestamp: clock::elapsed(),
            by_task_id,
            target_task_ids,
            operation,
            wait_id,
        });
    });
}

pin_project_lite::pin_project! {
    pub(crate) struct NamedFuture<F> {
        name: Option<crate::Str>,
        #[pin]
        future: F,
    }
}

impl<F: Future> Future for NamedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Some(name) = this.name.take()
            && let Some(task_id) = current_task_id()
        {
            record_task_name(task_id, &name);
        }
        this.future.poll(cx)
    }
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn named<F>(
    world: super::World,
    name: impl Into<crate::Str>,
    future: F,
) -> NamedFuture<F> {
    let name = if world.is_model() && is_active() {
        Some(name.into())
    } else {
        None
    };
    NamedFuture { name, future }
}

#[cfg(feature = "tracing")]
pub(crate) fn named_str<F>(world: super::World, name: crate::Str, future: F) -> NamedFuture<F> {
    let name = if world.is_model() && is_active() {
        Some(name)
    } else {
        None
    };
    NamedFuture { name, future }
}
