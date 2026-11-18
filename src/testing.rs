//! Deterministic interleaving tests, enabled by the `deterministic` feature.
//!
//! Models [`crate::Actor`], [`crate::Channel`], [`crate::Fanout`], [`crate::Reply`],
//! [`crate::Watch`],
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
//! [`check`] defaults to seeded random scheduling. Use [`Config::with_scheduler`]
//! to select probabilistic concurrency testing (PCT) or bounded depth-first
//! search (DFS). [`Replay::dfs_status`] reports whether DFS explored every
//! reachable schedule or reached the execution limit with choices remaining.
//!
//! A failure returns and prints a payload-free [`Replay`] with the scheduler,
//! bounds, engine versions and exact schedule. Save records with [`Replay::save`]
//! and use [`replay_corpus`] to exercise them against one scenario. Replay
//! requires compatible dependencies, scheduling behavior and fixture inputs.
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
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use shuttle::scheduler::{
    DataSource, DfsScheduler, PctScheduler, RandomDataSource, RandomScheduler, Schedule, Scheduler,
    Task, TaskId,
};

use crate::runtime::execution::Execution;

mod duplex;
pub use duplex::{
    DuplexConfig, DuplexControl, DuplexStream, LinkConfig, TransportFault, duplex,
    duplex_controlled, duplex_with,
};

const ENGINE: &str = "shuttle=0.9.10;engine=0.1.8;std=0.1.5;schedulers=0.1.1";
const FORMAT: u32 = 3;

/// Bounds for schedule exploration. Every execution gets fresh factory state.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Seed for random and PCT scheduling. DFS uses a fixed data seed.
    pub seed: u64,
    /// Maximum number of executions to explore. Must be nonzero.
    pub iterations: usize,
    /// Scheduling step limit per execution. Must be nonzero.
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

impl Config {
    /// Selects the scheduler for [`check`]. Passing a `Config` directly uses random scheduling.
    pub fn with_scheduler(self, scheduler: SchedulerMode) -> Exploration {
        Exploration {
            config: self,
            scheduler,
        }
    }
}

/// The scheduler and its exploration bounds, configured with [`Config::with_scheduler`].
#[derive(Clone, Copy, Debug)]
pub struct Exploration {
    config: Config,
    scheduler: SchedulerMode,
}

impl From<Config> for Exploration {
    fn from(config: Config) -> Self {
        config.with_scheduler(SchedulerMode::Random)
    }
}

/// How [`check`] chooses which modeled task runs next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerMode {
    /// Choose among runnable tasks using the configured seed.
    Random,
    /// Probabilistic concurrency testing. Randomize priorities after a calibration execution.
    Pct {
        /// At most `depth - 1` sampled priority changes per execution. Must be nonzero.
        /// An explicit yield may also lower a task's priority.
        depth: usize,
    },
    /// Enumerate task choices in depth-first order with the same random data stream each time.
    Dfs,
}

/// Coverage of a DFS check whose explored schedules all passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DfsStatus {
    /// Every reachable modeled schedule for the fixed inputs passed.
    Complete,
    /// The execution limit was reached with schedules still to explore.
    IterationLimit,
}

/// A payload-free, versioned execution record. Use [`replay`] to run it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replay {
    format: u32,
    engine: String,
    #[serde(deserialize_with = "deserialize_scheduler")]
    scheduler: SchedulerMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dfs_status: Option<DfsStatus>,
    seed: u64,
    iterations: usize,
    iteration: usize,
    max_steps: usize,
    data_seed: u64,
    // None means a scheduler-owned random draw, Some identifies a logical task.
    steps: Vec<Option<usize>>,
}

impl Replay {
    fn new(exploration: Exploration) -> Self {
        Self {
            format: FORMAT,
            engine: ENGINE.into(),
            scheduler: exploration.scheduler,
            dfs_status: None,
            seed: exploration.config.seed,
            iterations: exploration.config.iterations,
            iteration: 0,
            max_steps: exploration.config.max_steps,
            data_seed: 0,
            steps: Vec::new(),
        }
    }

    /// The original exploration scheduler, including any mode-specific bound.
    pub fn scheduler(&self) -> SchedulerMode {
        self.scheduler
    }

    /// Coverage of the original DFS check. Absent for other modes and for a
    /// DFS check that found a failing schedule.
    pub fn dfs_status(&self) -> Option<DfsStatus> {
        self.dfs_status
    }

    /// The configured exploration seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The configured limit on the number of executions.
    pub fn iterations(&self) -> usize {
        self.iterations
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

    /// Loads a record. Invalid data errors identify incompatible metadata.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Header {
            format: u32,
        }

        let bytes = std::fs::read(path)?;
        // Read the version before requiring fields that older records lack.
        let header: Header = serde_json::from_slice(&bytes).map_err(invalid_replay)?;
        validate_format(header.format)?;
        let record: Self = serde_json::from_slice(&bytes).map_err(invalid_replay)?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> io::Result<()> {
        validate_format(self.format)?;
        if self.engine != ENGINE {
            return Err(invalid_replay(format_args!(
                "engine is {:?}, expected {ENGINE:?}",
                self.engine
            )));
        }
        if self.iterations == 0 {
            return Err(invalid_replay("iterations must be nonzero"));
        }
        if !(1..=self.iterations).contains(&self.iteration) {
            return Err(invalid_replay(format_args!(
                "iteration is {}, expected 1..={}",
                self.iteration, self.iterations
            )));
        }
        if self.max_steps == 0 {
            return Err(invalid_replay("max_steps must be nonzero"));
        }
        if let SchedulerMode::Pct { depth: 0 } = self.scheduler {
            return Err(invalid_replay("scheduler.pct.depth must be nonzero"));
        }
        if self.dfs_status.is_some() && self.scheduler != SchedulerMode::Dfs {
            return Err(invalid_replay("dfs_status requires the DFS scheduler"));
        }
        if self.dfs_status == Some(DfsStatus::IterationLimit) && self.iteration != self.iterations {
            return Err(invalid_replay(
                "dfs_status iteration_limit requires the final iteration",
            ));
        }
        if !matches!(self.steps.first(), Some(Some(_))) {
            return Err(invalid_replay("steps must start with a task choice"));
        }
        Ok(())
    }
}

fn deserialize_scheduler<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SchedulerMode, D::Error> {
    SchedulerMode::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format_args!("scheduler: {error}")))
}

fn validate_format(format: u32) -> io::Result<()> {
    if format != FORMAT {
        return Err(invalid_replay(format_args!(
            "format is {format}, expected {FORMAT}"
        )));
    }
    Ok(())
}

fn invalid_replay(message: impl fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid adamantium::testing replay: {message}"),
    )
}

/// Why an execution failed.
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

/// Explores schedules up to [`Config::iterations`] and returns the last successful record.
/// Stops on the first failure. The factory runs anew inside each execution.
///
/// Random and PCT checks pass when their execution budget is spent. The first
/// PCT execution calibrates the scheduler and counts against that budget. If
/// calibration offers no task choices, the check ends after that execution.
/// A successful DFS check reports either [`DfsStatus::Complete`] or
/// [`DfsStatus::IterationLimit`] in its record. Any execution that exceeds
/// [`Config::max_steps`] fails with [`FailureKind::StepLimit`].
///
/// Panics for zero bounds, a nested harness, a current Tokio runtime, or Shuttle
/// environment overrides that would silently change the recorded seed.
pub fn check<F, Fut>(config: impl Into<Exploration>, factory: F) -> Result<Replay, Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let exploration = config.into();
    let Config {
        seed,
        iterations,
        max_steps,
    } = exploration.config;
    assert!(
        iterations > 0 && max_steps > 0,
        "adamantium::testing bounds must be nonzero"
    );
    assert!(
        !matches!(exploration.scheduler, SchedulerMode::Pct { depth: 0 }),
        "adamantium::testing PCT depth must be nonzero"
    );
    assert!(
        std::env::var_os("SHUTTLE_RANDOM_SEED").is_none()
            && std::env::var_os("SHUTTLE_ALWAYS_PERSIST_SEED").is_none(),
        "adamantium::testing uses Config::seed and Replay::save; remove Shuttle environment overrides"
    );
    let record = Replay::new(exploration);
    match exploration.scheduler {
        SchedulerMode::Random => run(
            record,
            RandomScheduler::new_from_seed(seed, iterations),
            None,
            factory,
        ),
        SchedulerMode::Pct { depth } => run(
            record,
            PctScheduler::new_from_seed(seed, depth, iterations),
            None,
            factory,
        ),
        SchedulerMode::Dfs => run(record, DfsScheduler::new(None, true), None, factory),
    }
}

/// Replays one saved execution with fresh state and the original step limit.
/// Incompatible metadata panics before the scenario runs. The fixture inputs
/// must match the original run. A change to the schedule fails explicitly.
pub fn replay<F, Fut>(record: &Replay, factory: F) -> Result<Replay, Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let validation = record.validate();
    assert!(validation.is_ok(), "{validation:?}");
    play(record, ReplayPolicy::Exact, factory)
}

/// Why replaying a saved corpus stopped. Each variant identifies the failing file.
#[derive(Debug)]
pub enum CorpusError {
    /// The record could not be read or was incompatible.
    Load {
        /// The record's path.
        path: PathBuf,
        /// The loading or validation error.
        source: io::Error,
    },
    /// The loaded schedule failed or diverged during replay.
    Execution {
        /// The record's path.
        path: PathBuf,
        /// The failure reported by the replay.
        source: Box<Failure>,
    },
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { path, source } => write!(f, "load {}: {source}", path.display()),
            Self::Execution { path, source } => write!(f, "replay {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for CorpusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load { source, .. } => Some(source),
            Self::Execution { source, .. } => Some(source.as_ref()),
        }
    }
}

/// Replays each saved schedule in input order with fresh state from the same factory.
/// Once all recorded steps match, round-robin scheduling can finish a scenario
/// that now runs past its original failure. The original step limit applies.
/// Returns the first load error, execution failure or schedule mismatch with its path.
/// An empty corpus passes.
pub fn replay_corpus<P, F, Fut>(
    paths: impl IntoIterator<Item = P>,
    factory: F,
) -> Result<(), CorpusError>
where
    P: AsRef<Path>,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let factory = Arc::new(factory);
    for path in paths {
        let path = path.as_ref();
        let record = Replay::load(path).map_err(|source| CorpusError::Load {
            path: path.to_owned(),
            source,
        })?;
        let factory = Arc::clone(&factory);
        play(&record, ReplayPolicy::Prefix, move || factory()).map_err(|source| {
            CorpusError::Execution {
                path: path.to_owned(),
                source: Box::new(source),
            }
        })?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ReplayPolicy {
    Exact,
    Prefix,
}

fn play<F, Fut>(record: &Replay, policy: ReplayPolicy, factory: F) -> Result<Replay, Failure>
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

struct Playback {
    steps: std::vec::IntoIter<Option<usize>>,
    data: RandomDataSource,
    started: bool,
    policy: ReplayPolicy,
}

impl Playback {
    fn next_step(&mut self) -> Option<Option<usize>> {
        let step = self.steps.next();
        assert!(
            step.is_some() || matches!(self.policy, ReplayPolicy::Prefix),
            "schedule ended early"
        );
        step
    }
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
        let step = self.next_step();
        assert!(!matches!(step, Some(None)), "expected context switch");
        if let Some(task) = step.flatten().map(TaskId::from) {
            assert!(
                runnable.iter().any(|candidate| candidate.id() == task),
                "scheduled task is not runnable: {task:?}"
            );
            return Some(task);
        }
        // Prefer the next higher task ID, wrapping to the lowest runnable ID.
        runnable
            .iter()
            .map(|task| task.id())
            .min_by_key(|id| (current.is_some_and(|current| *id <= current), *id))
    }

    fn next_u64(&mut self) -> u64 {
        assert!(
            !matches!(self.next_step(), Some(Some(_))),
            "expected random choice"
        );
        self.data.next_u64()
    }
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
        if !self.replaying && trace.scheduler == SchedulerMode::Dfs {
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
        drop(trace);
        Execution::next();
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
        Some(task)
    }

    fn next_u64(&mut self) -> u64 {
        let value = self.scheduler.next_u64();
        lock(&self.trace).steps.push(None);
        value
    }
}

fn run<S, F, Fut>(
    record: Replay,
    scheduler: S,
    original: Option<(&Replay, ReplayPolicy)>,
    factory: F,
) -> Result<Replay, Failure>
where
    S: Scheduler + 'static,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let _execution = Execution::enter();
    let max_steps = record.max_steps;
    let trace = Arc::new(Mutex::new(record));
    let scheduler = Recording {
        scheduler,
        trace: Arc::clone(&trace),
        replaying: original.is_some(),
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
            shuttle::future::block_on(factory());
        });
    }));
    let record = lock(&trace).clone();
    let kind = if original.is_some_and(|(original, policy)| match policy {
        ReplayPolicy::Exact => original.steps != record.steps,
        ReplayPolicy::Prefix => !record.steps.starts_with(&original.steps),
    }) {
        FailureKind::ReplayMismatch
    } else {
        match result {
            Ok(_) => return Ok(record),
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

// Shuttle exposes engine failures as panic strings. Keep the prefixes covered by
// integration tests and version pins. Never serialize payloads.
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
