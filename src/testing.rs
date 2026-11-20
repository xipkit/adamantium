#![allow(
    clippy::disallowed_types,
    reason = "scheduler bookkeeping is outside the modeled program"
)]

use std::{
    any::Any,
    collections::BTreeMap,
    fmt,
    future::Future,
    io,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
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

/// Bounds for schedule exploration.
///
/// `seed` controls random and PCT scheduling. DFS uses a fixed data seed.
/// `iterations` and `max_steps` must be nonzero.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub seed: u64,
    pub iterations: usize,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerMode {
    Random,
    Pct { depth: usize },
    Dfs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DfsStatus {
    Complete,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Panic,
    Deadlock,
    StepLimit,
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

/// Explores schedules and returns the last successful record or the first failure.
/// Create fresh state in the factory for each execution. Use modeled primitives
/// and virtual time in place of real I/O and wall-clock waits.
///
/// Random and PCT checks stop at the iteration limit. PCT counts calibration
/// as one execution and stops early if it finds no task choices. Inspect
/// [`Replay::dfs_status`] to see whether DFS covered every reachable schedule.
/// Exceeding `max_steps` returns [`FailureKind::StepLimit`]. The limit counts
/// scheduling operations and cannot stop a CPU-only loop.
///
/// # Panics
///
/// Panics for zero bounds, a nested harness, an active Tokio runtime, or
/// Shuttle environment overrides that change the seed.
///
/// ```
/// use adamantium::{Actor, testing::{self, Config}};
///
/// testing::check(Config::default(), || async {
///     let count = Actor::new(0);
///     let workers = (0..2).map(|_| {
///         let count = count.clone();
///         async move { count.with(|n| *n += 1); }
///     }).collect::<Vec<_>>();
///     testing::parallel(workers).await;
///     assert_eq!(count.into_inner().ok(), Some(2));
/// }).unwrap();
/// ```
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
}

/// Payload-free named task timeline reconstructed from a [`Replay`].
#[derive(Clone, PartialEq, Eq)]
pub struct Timeline {
    failure: Option<FailureKind>,
    replay: Replay,
    tasks: Vec<TaskInfo>,
    waiting: Vec<WaitingTask>,
    events: Vec<TimelineEvent>,
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
            Some(kind) => writeln!(
                f,
                "adamantium::testing {kind:?} (scheduler={:?}, seed={}, iteration={}, max_steps={}):",
                self.replay.scheduler,
                self.replay.seed,
                self.replay.iteration,
                self.replay.max_steps
            )?,
            None => writeln!(
                f,
                "adamantium::testing Passed (scheduler={:?}, seed={}, iteration={}, max_steps={}):",
                self.replay.scheduler,
                self.replay.seed,
                self.replay.iteration,
                self.replay.max_steps
            )?,
        }
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
                    writeln!(f, " scheduled")?;
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

/// Replays `record` with timeline tracing enabled to reconstruct task names,
/// virtual timestamps, wait operations and wakeups without altering the schedule.
///
/// The returned [`Timeline`] excludes all user payloads and panic messages.
pub fn explain<F, Fut>(record: &Replay, factory: F) -> Timeline
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    use crate::runtime::trace::{Guard, RawEvent};

    let validation = record.validate();
    assert!(validation.is_ok(), "{validation:?}");

    let guard = Guard::enter();
    let result = play(record, ReplayPolicy::Exact, factory);
    let collector = guard.finish();

    let (failure, observed_replay) = match result {
        Ok(replay) => (None, replay),
        Err(failure) => (Some(failure.kind()), failure.replay().clone()),
    };

    let Some(collector) = collector else {
        return Timeline {
            failure,
            replay: observed_replay,
            tasks: Vec::new(),
            waiting: Vec::new(),
            events: Vec::new(),
        };
    };

    let names: BTreeMap<usize, String> = collector.names;
    let name_of = |id: usize| names.get(&id).cloned();

    let mut task_ids: BTreeMap<usize, Option<String>> = names
        .iter()
        .map(|(id, name)| (*id, Some(name.clone())))
        .collect();

    let mut events = Vec::with_capacity(collector.events.len() + usize::from(failure.is_some()));
    let mut last_timestamp = Duration::ZERO;
    for raw in collector.events {
        let (timestamp, kind) = match raw {
            RawEvent::Scheduled { timestamp, task_id } => {
                task_ids.entry(task_id).or_insert_with(|| name_of(task_id));
                (
                    timestamp,
                    TimelineEventKind::Scheduled {
                        task_id,
                        task_name: name_of(task_id),
                    },
                )
            }
            RawEvent::Wait {
                timestamp,
                task_id,
                operation,
                wait_id,
            } => {
                task_ids.entry(task_id).or_insert_with(|| name_of(task_id));
                (
                    timestamp,
                    TimelineEventKind::Wait {
                        task_id,
                        task_name: name_of(task_id),
                        operation,
                        wait_id,
                    },
                )
            }
            RawEvent::Wakeup {
                timestamp,
                by_task_id,
                target_task_ids,
                operation,
                wait_id,
            } => {
                if let Some(id) = by_task_id {
                    task_ids.entry(id).or_insert_with(|| name_of(id));
                }
                for id in &target_task_ids {
                    task_ids.entry(*id).or_insert_with(|| name_of(*id));
                }
                let target_task_names = target_task_ids.iter().map(|id| name_of(*id)).collect();
                (
                    timestamp,
                    TimelineEventKind::Wakeup {
                        by_task_id,
                        by_task_name: by_task_id.and_then(name_of),
                        target_task_ids,
                        target_task_names,
                        operation,
                        wait_id,
                    },
                )
            }
            RawEvent::WaitCompleted {
                timestamp,
                task_id,
                operation,
                wait_id,
            } => {
                task_ids.entry(task_id).or_insert_with(|| name_of(task_id));
                (
                    timestamp,
                    TimelineEventKind::WaitCompleted {
                        task_id,
                        task_name: name_of(task_id),
                        operation,
                        wait_id,
                    },
                )
            }
            RawEvent::WaitCancelled {
                timestamp,
                task_id,
                operation,
                wait_id,
            } => {
                task_ids.entry(task_id).or_insert_with(|| name_of(task_id));
                (
                    timestamp,
                    TimelineEventKind::WaitCancelled {
                        task_id,
                        task_name: name_of(task_id),
                        operation,
                        wait_id,
                    },
                )
            }
        };
        last_timestamp = timestamp;
        events.push(TimelineEvent { timestamp, kind });
    }

    if let Some(kind) = failure {
        let task_id = collector.last_scheduled;
        if let Some(id) = task_id {
            task_ids.entry(id).or_insert_with(|| name_of(id));
        }
        events.push(TimelineEvent {
            timestamp: last_timestamp,
            kind: TimelineEventKind::Failed {
                kind,
                task_id,
                task_name: task_id.and_then(name_of),
            },
        });
    }

    let tasks = task_ids
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
            task_name: name_of(entry.task_id),
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

#[derive(Debug)]
pub enum CorpusError {
    Load { path: PathBuf, source: io::Error },
    Execution { path: PathBuf, source: Box<Failure> },
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
        .map(crate::runtime::coop::model::task)
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
            shuttle::future::block_on(crate::runtime::coop::model::task(factory()));
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
