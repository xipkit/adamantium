use std::{
    any::Any,
    future::Future,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex, PoisonError},
};

use shuttle::scheduler::{DataSource, RandomDataSource, Schedule, Scheduler, Task, TaskId};

use super::{DfsStatus, Failure, FailureKind, Replay, SchedulerMode, build_timeline};
use crate::runtime::{
    execution::Execution,
    trace::{Collector, Guard},
};

#[derive(Clone, Copy)]
pub(super) enum ReplayPolicy {
    Exact,
    Prefix,
    Shrink,
}

pub(super) fn play<F, Fut>(
    record: &Replay,
    policy: ReplayPolicy,
    factory: F,
) -> Result<(Replay, Option<Collector>), Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let scheduler = Playback {
        steps: record.steps.clone().into_iter(),
        data: RandomDataSource::initialize(record.data_seed),
        started: false,
        policy,
    };
    run(record.clone(), scheduler, Some((record, policy)), factory)
}

/// Follows a recorded schedule, one step per scheduling decision.
///
/// It never panics when the execution departs from the record. A panic here
/// fails the execution inside Shuttle, which leaks any task suspended partway
/// through unwinding and leaves the thread panicking. Instead, a step it
/// can't follow gives way to the fallback order used past the end of the
/// record, the execution ends normally, and `run` compares the executed steps
/// with the record.
struct Playback {
    steps: std::vec::IntoIter<Option<usize>>,
    data: RandomDataSource,
    started: bool,
    policy: ReplayPolicy,
}

impl Scheduler for Playback {
    fn new_execution(&mut self) -> Option<Schedule> {
        if std::mem::replace(&mut self.started, true) {
            return None;
        }
        Some(Schedule::new(self.data.reinitialize()))
    }

    fn next_task(
        &mut self,
        runnable: &[&Task],
        current: Option<TaskId>,
        _yielding: bool,
    ) -> Option<TaskId> {
        let step = self.steps.next().flatten().map(TaskId::from);
        if let Some(task) = step
            && runnable.iter().any(|candidate| candidate.id() == task)
        {
            return Some(task);
        }
        if matches!(self.policy, ReplayPolicy::Shrink)
            && let Some(current) = current
            && runnable.iter().any(|task| task.id() == current)
        {
            return Some(current);
        }
        runnable
            .iter()
            .map(|task| task.id())
            .min_by_key(|id| (current.is_some_and(|current| *id <= current), *id))
    }

    fn next_u64(&mut self) -> u64 {
        // The value comes from the data seed. The step only marks the draw.
        self.steps.next();
        self.data.next_u64()
    }
}

fn lock(trace: &Mutex<Replay>) -> std::sync::MutexGuard<'_, Replay> {
    trace.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Recording<S> {
    scheduler: S,
    trace: Arc<Mutex<Replay>>,
    replaying: bool,
    saw_choice: bool,
}

impl<S: Scheduler> Scheduler for Recording<S> {
    fn new_execution(&mut self) -> Option<Schedule> {
        let mut trace = lock(&self.trace);
        // PCT needs a contended scheduling decision to initialize priorities
        // after calibration. With no task choices there is no path to vary.
        if !self.replaying
            && matches!(trace.scheduler, SchedulerMode::Pct { .. })
            && trace.iteration == 1
            && !self.saw_choice
        {
            return None;
        }
        let schedule = self.scheduler.new_execution();
        if !self.replaying && trace.scheduler.is_dfs() {
            // Query before checking the limit so a tree that finishes on the
            // final allowed execution counts as complete.
            if schedule.is_none() {
                trace.dfs_status = Some(DfsStatus::Complete);
            } else if trace.iteration == trace.iterations {
                trace.dfs_status = Some(DfsStatus::IterationLimit);
                return None;
            }
        }
        let schedule = schedule?;
        if !self.replaying {
            trace.iteration += 1;
        }
        trace.data_seed = schedule.seed;
        trace.steps.clear();
        let wall_epoch = trace.wall_epoch;
        drop(trace);
        Execution::next(wall_epoch);
        Guard::reset();
        Some(schedule)
    }

    fn next_task(
        &mut self,
        runnable: &[&Task],
        current: Option<TaskId>,
        yielding: bool,
    ) -> Option<TaskId> {
        let runnable = crate::runtime::clock::runnable(runnable);
        self.saw_choice |= runnable.len() > 1;
        let task = self.scheduler.next_task(&runnable, current, yielding)?;
        lock(&self.trace).steps.push(Some(usize::from(task)));
        if crate::runtime::trace::is_active() {
            let shuttle_name = runnable
                .iter()
                .find(|candidate| candidate.id() == task)
                .and_then(|candidate| candidate.name());
            crate::runtime::trace::record_scheduled(usize::from(task), shuttle_name);
        }
        Some(task)
    }

    fn next_u64(&mut self) -> u64 {
        let value = self.scheduler.next_u64();
        lock(&self.trace).steps.push(None);
        value
    }
}

pub(super) fn run<S, F, Fut>(
    record: Replay,
    scheduler: S,
    original: Option<(&Replay, ReplayPolicy)>,
    factory: F,
) -> Result<(Replay, Option<Collector>), Failure>
where
    S: Scheduler + Send + 'static,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let replaying = original.is_some();
    let (record, collector, result) =
        Execution::isolated(move || execute(record, scheduler, replaying, factory));
    let kind = if original.is_some_and(|(original, policy)| match policy {
        ReplayPolicy::Exact => original.steps != record.steps,
        ReplayPolicy::Prefix => !record.steps.starts_with(&original.steps),
        // The candidate may fail before consuming all of its proposed steps.
        ReplayPolicy::Shrink => false,
    }) {
        FailureKind::ReplayMismatch
    } else {
        match &result {
            Ok(_) => return Ok((record, collector)),
            Err(payload) => classify(&**payload),
        }
    };
    let shrinking = matches!(original, Some((_, ReplayPolicy::Shrink)));
    let shrink_message = if shrinking {
        result
            .as_ref()
            .err()
            .and_then(|payload| panic_message(&**payload))
            .map(str::to_owned)
    } else {
        None
    };
    let failure = Failure {
        kind,
        shrink_message,
        timeline: Box::new(build_timeline(record, Some(kind), collector)),
    };
    if !shrinking {
        eprintln!("{failure}");
    }
    Err(failure)
}

/// Runs the executions of one harness call on the current thread and returns
/// the last execution's record, its trace and how it ended.
fn execute<S, F, Fut>(
    record: Replay,
    scheduler: S,
    replaying: bool,
    factory: F,
) -> (Replay, Option<Collector>, std::thread::Result<()>)
where
    S: Scheduler + 'static,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let guard = Guard::enter();
    let max_steps = record.max_steps;
    let trace = Arc::new(Mutex::new(record));
    let scheduler = Recording {
        scheduler,
        trace: Arc::clone(&trace),
        replaying,
        saw_choice: false,
    };
    let mut engine = shuttle::Config::default();
    // Nested scope/select/cleanup polls exceed Shuttle's 60 KiB default in
    // Rust 1.88 debug builds. Match an ordinary worker's 2 MiB stack budget.
    engine.stack_size = 2 * 1024 * 1024;
    engine.max_steps = shuttle::MaxSteps::FailAfter(max_steps);
    engine.failure_persistence = shuttle::FailurePersistence::None;
    engine
        .ungraceful_shutdown_config
        .continuation_function_behavior = shuttle::ContinuationFunctionBehavior::Drop;
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        shuttle::Runner::new(scheduler, engine).run(move || {
            crate::runtime::clock::start();
            shuttle::future::block_on(crate::runtime::coop::model::task(factory()));
        });
    }));
    let collector = guard.finish();
    let record = lock(&trace).clone();
    (record, collector, result)
}

fn classify(payload: &(dyn Any + Send)) -> FailureKind {
    let message = panic_message(payload).unwrap_or("");
    if message.starts_with("exceeded max_steps bound ") {
        FailureKind::StepLimit
    } else if message.starts_with("deadlock! blocked tasks:") {
        FailureKind::Deadlock
    } else {
        FailureKind::Panic
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
}
