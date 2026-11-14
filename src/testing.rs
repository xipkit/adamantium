//! Deterministic interleaving tests, enabled by the `deterministic` feature.
//!
//! Models [`crate::Actor`], [`crate::Channel`], [`crate::Reply`], [`crate::Watch`],
//! [`crate::Tasks`] and [`crate::Cancellation`], including concurrent maps and
//! streams, result claiming, panic transport, scope abort and draining.
//! [`parallel`] also supplies independent workers. Blocking closures model the
//! queued/started boundary: abort can prevent entry, but started work finishes.
//! Replace actual blocking I/O with fixtures using modeled primitives.
//! [`crate::time`] drives virtual deadlines without wall-clock waits. Time advances
//! to the earliest pending deadline only when no application task can run.
//! OS signals remain unsupported.
//! Outside a harness execution, enabling the feature leaves production behavior
//! intact, including ordinary `#[tokio::test]` tests in the same build.
//!
//! Create fresh state **inside** the factory. A primitive belongs to the
//! execution that created it; carrying it into another execution, a native
//! thread, or back to production is an error. Use `#[test]`: starting the
//! harness inside a Tokio runtime or another harness panics.
//!
//! ```
//! use adamantium::{Actor, testing::{self, Config}};
//!
//! testing::check(Config::default(), || async {
//!     let count = Actor::new(0);
//!     let workers = (0..2).map(|_| {
//!         let count = count.clone();
//!         async move { count.with(|n| *n += 1); }
//!     }).collect::<Vec<_>>();
//!     testing::parallel(workers).await;
//!     assert_eq!(count.into_inner().ok(), Some(2));
//! }).unwrap();
//! ```
//!
//! A failure returns and prints a payload-free [`Replay`], including the seed,
//! one-based iteration, step limit and engine versions. Save it with
//! [`Replay::save`] and load it with [`Replay::load`] for [`replay`]. Replay
//! requires the same code, dependency versions and fixture inputs. Passing
//! seeded exploration does not prove that every possible execution is correct.
//!
//! Real I/O, native threads, wall-clock time, uncontrolled randomness and foreign
//! executors are outside the model: replace them with deterministic fixtures.
//! Arbitrary foreign futures cannot all be detected. A step limit counts
//! scheduling operations, not CPU instructions; it cannot stop a CPU-only loop.

#![allow(
    clippy::disallowed_types,
    reason = "scheduler bookkeeping is outside the modeled program"
)]

use std::{
    any::Any,
    fmt,
    future::Future,
    io,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use shuttle::scheduler::{RandomScheduler, ReplayScheduler, Schedule, Scheduler, Task, TaskId};

use crate::runtime::execution::Execution;

const ENGINE: &str = "shuttle=0.9.10;engine=0.1.8;std=0.1.5;schedulers=0.1.1";
// The clock driver changes task identities and scheduling choices in v1 traces.
const FORMAT: u32 = 2;

/// Bounds for seeded exploration. Every execution gets fresh factory state.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Seed for scheduling choices. Independent of wall-clock time.
    pub seed: u64,
    /// Number of executions to explore; must be nonzero.
    pub iterations: usize,
    /// Maximum scheduling steps per execution; exhaustion fails the test.
    pub max_steps: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: 0,
            iterations: 100,
            max_steps: 10_000,
        }
    }
}

/// A payload-free, versioned execution record. Use [`replay`] to run it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replay {
    format: u32,
    engine: String,
    seed: u64,
    iteration: usize,
    max_steps: usize,
    data_seed: u64,
    // None means a scheduler-owned random draw, Some identifies a logical task.
    steps: Vec<Option<usize>>,
}

impl Replay {
    /// The configured exploration seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The one-based iteration in the original exploration.
    pub fn iteration(&self) -> usize {
        self.iteration
    }

    /// The scheduling step limit for this execution.
    pub fn max_steps(&self) -> usize {
        self.max_steps
    }

    /// The exact pinned scheduling engine and component versions.
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// Saves scheduling metadata only; no state or panic payload is serialized.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        std::fs::write(path, bytes)
    }

    /// Loads a record, rejecting incompatible formats or scheduling engines.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let record: Self =
            serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> io::Result<()> {
        if self.format != FORMAT
            || self.engine != ENGINE
            || self.iteration == 0
            || self.max_steps == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incompatible adamantium::testing replay metadata",
            ));
        }
        Ok(())
    }

    fn schedule(&self) -> Schedule {
        let mut schedule = Schedule::new(self.data_seed);
        for step in &self.steps {
            match step {
                Some(task) => schedule.push_task(TaskId::from(*task)),
                None => schedule.push_random(),
            }
        }
        schedule
    }
}

/// Why an execution failed. A bound exhaustion is never a passing execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// An assertion, primitive misuse or other panic in the scenario.
    Panic,
    /// Unfinished modeled tasks cannot make progress.
    Deadlock,
    /// The configured scheduling step budget was exhausted.
    StepLimit,
    /// The scenario no longer follows the supplied schedule.
    ReplayMismatch,
}

/// A failed execution and its replay record. Debug output excludes user payloads.
pub struct Failure {
    kind: FailureKind,
    replay: Replay,
}

impl Failure {
    /// The failure category.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// The observed execution record, suitable for [`replay`] or [`Replay::save`].
    /// On a replay mismatch, this contains the steps actually executed.
    pub fn replay(&self) -> &Replay {
        &self.replay
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "adamantium::testing {:?}: ", self.kind)?;
        let record = serde_json::to_string(&self.replay).map_err(|_| fmt::Error)?;
        f.write_str(&record)
    }
}

impl std::error::Error for Failure {}

/// Explores `config.iterations` schedules and returns the last successful record.
/// Stops on the first failure. The factory runs anew inside each execution.
///
/// Panics for zero bounds, a nested harness, a current Tokio runtime, or Shuttle
/// environment overrides that would silently change the recorded seed.
pub fn check<F, Fut>(config: Config, factory: F) -> Result<Replay, Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    assert!(
        config.iterations > 0 && config.max_steps > 0,
        "adamantium::testing bounds must be nonzero"
    );
    assert!(
        std::env::var_os("SHUTTLE_RANDOM_SEED").is_none()
            && std::env::var_os("SHUTTLE_ALWAYS_PERSIST_SEED").is_none(),
        "adamantium::testing uses Config::seed and Replay::save; remove Shuttle environment overrides"
    );
    run(
        config,
        RandomScheduler::new_from_seed(config.seed, config.iterations),
        None,
        factory,
    )
}

/// Replays one saved execution with fresh state and the original step limit.
/// Incompatible metadata panics before the scenario runs. Code and all fixture
/// inputs must match the original run; changed schedules fail explicitly.
pub fn replay<F, Fut>(record: &Replay, factory: F) -> Result<Replay, Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    assert!(
        record.validate().is_ok(),
        "incompatible adamantium::testing replay metadata"
    );
    run(
        Config {
            seed: record.seed,
            iterations: 1,
            max_steps: record.max_steps,
        },
        ReplayScheduler::new_from_schedule(record.schedule()),
        Some(record.clone()),
        factory,
    )
}

/// Runs independent test workers and joins their results in input order.
///
/// Only usable inside this harness. This is a primitive test fixture, not a model
/// of Tasks' error/cancellation semantics. A worker panic fails the execution.
/// Each worker must own its inputs, as a spawned production task does.
pub async fn parallel<I, F>(workers: I) -> Vec<F::Output>
where
    I: IntoIterator<Item = F>,
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    assert!(
        crate::runtime::execution::current().is_some(),
        "adamantium::testing::parallel requires the testing harness"
    );
    let handles = workers
        .into_iter()
        .map(shuttle::future::spawn)
        .collect::<Vec<_>>();
    let mut values = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(value) => values.push(value),
            Err(error) => panic::resume_unwind(Box::new(error)),
        }
    }
    values
}

struct Trace {
    record: Replay,
}

fn lock(trace: &Mutex<Trace>) -> std::sync::MutexGuard<'_, Trace> {
    trace.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Recording<S> {
    scheduler: S,
    trace: Arc<Mutex<Trace>>,
    replaying: bool,
}

impl<S: Scheduler> Scheduler for Recording<S> {
    fn new_execution(&mut self) -> Option<Schedule> {
        let schedule = self.scheduler.new_execution()?;
        Execution::next();
        let mut trace = lock(&self.trace);
        if !self.replaying {
            trace.record.iteration += 1;
        }
        trace.record.data_seed = schedule.seed;
        trace.record.steps.clear();
        Some(schedule)
    }

    fn next_task(
        &mut self,
        runnable: &[&Task],
        current: Option<TaskId>,
        yielding: bool,
    ) -> Option<TaskId> {
        let runnable = crate::runtime::clock::runnable(runnable);
        let task = self.scheduler.next_task(&runnable, current, yielding)?;
        lock(&self.trace).record.steps.push(Some(usize::from(task)));
        Some(task)
    }

    fn next_u64(&mut self) -> u64 {
        let value = self.scheduler.next_u64();
        lock(&self.trace).record.steps.push(None);
        value
    }
}

fn run<S, F, Fut>(
    config: Config,
    scheduler: S,
    original: Option<Replay>,
    factory: F,
) -> Result<Replay, Failure>
where
    S: Scheduler + 'static,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let _execution = Execution::enter();
    let trace = Arc::new(Mutex::new(Trace {
        record: Replay {
            format: FORMAT,
            engine: ENGINE.into(),
            seed: config.seed,
            iteration: original.as_ref().map_or(0, |r| r.iteration),
            max_steps: config.max_steps,
            data_seed: 0,
            steps: Vec::new(),
        },
    }));
    let scheduler = Recording {
        scheduler,
        trace: Arc::clone(&trace),
        replaying: original.is_some(),
    };
    let mut engine = shuttle::Config::default();
    // Nested scope/select/cleanup polls exceed Shuttle's 60 KiB default in
    // Rust 1.88 debug builds. Match an ordinary worker's 2 MiB stack budget.
    engine.stack_size = 2 * 1024 * 1024;
    engine.max_steps = shuttle::MaxSteps::FailAfter(config.max_steps);
    engine.failure_persistence = shuttle::FailurePersistence::None;
    engine
        .ungraceful_shutdown_config
        .continuation_function_behavior = shuttle::ContinuationFunctionBehavior::Drop;
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        shuttle::Runner::new(scheduler, engine).run(move || {
            crate::runtime::clock::start();
            shuttle::future::block_on(factory());
        });
    }));
    let record = lock(&trace).record.clone();
    let kind = if original.as_ref().is_some_and(|r| r.steps != record.steps) {
        FailureKind::ReplayMismatch
    } else {
        match result {
            Ok(()) => return Ok(record),
            Err(payload) => classify(&*payload, original.is_some()),
        }
    };
    let failure = Failure {
        kind,
        replay: record,
    };
    eprintln!("{failure}");
    Err(failure)
}

// Shuttle exposes engine failures as panic strings. Keep these exact prefixes
// covered by integration tests and version pins, and never serialize payloads.
fn classify(payload: &(dyn Any + Send), replaying: bool) -> FailureKind {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("");
    if message.starts_with("exceeded max_steps bound ") {
        FailureKind::StepLimit
    } else if message.starts_with("deadlock! blocked tasks:") {
        FailureKind::Deadlock
    } else if replaying
        && (message.starts_with("schedule ended early")
            || message.starts_with("scheduled task is not runnable")
            || message.starts_with("expected context switch")
            || message.starts_with("expected random choice"))
    {
        FailureKind::ReplayMismatch
    } else {
        FailureKind::Panic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Actor;
    use shuttle::scheduler::DfsScheduler;

    #[test]
    fn exhausts_two_actor_updates() {
        let last = run(
            Config {
                seed: 0,
                iterations: 1,
                max_steps: 100,
            },
            DfsScheduler::new(None, false),
            None,
            || async {
                let count = Arc::new(Actor::new(0));
                let worker = Arc::clone(&count);
                let child = shuttle::thread::spawn(move || worker.with(|n| *n += 1));
                count.with(|n| *n += 1);
                child.join().unwrap();
                assert_eq!(count.snapshot(), 2);
            },
        )
        .unwrap();
        assert!(last.iteration() > 1);
    }
}
