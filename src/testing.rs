#![allow(
    clippy::disallowed_types,
    reason = "scheduler bookkeeping is outside the modeled program"
)]

use std::{
    fmt,
    future::Future,
    io, panic,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use shuttle::scheduler::{DfsScheduler, PctScheduler, RandomScheduler, Scheduler};

use crate::runtime::trace;

mod config;
mod duplex;
mod engine;
mod fault;
mod record;
mod shrink;
mod timeline;
pub use config::{Config, DfsStatus, Exploration, SchedulerMode};
pub use duplex::{
    DuplexConfig, DuplexControl, DuplexStream, LinkConfig, TransportFault, duplex,
    duplex_controlled, duplex_with,
};
use engine::{ReplayPolicy, play, run};
pub use fault::{FaultInjector, FaultRule, FaultStep, Latency};
pub use record::Replay;
pub use shrink::shrink;
use timeline::build_timeline;
pub use timeline::{TaskInfo, Timeline, TimelineEvent, TimelineEventKind, WaitingTask};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Panic,
    Deadlock,
    StepLimit,
    ReplayMismatch,
    /// A [`SchedulerMode::Dfs`] check ran `iterations` executions and
    /// schedules remained. Every execution it ran passed.
    IterationLimit,
}

/// A failed execution with a named task timeline and a replay record.
/// Display and debug output exclude user payloads.
pub struct Failure {
    kind: FailureKind,
    timeline: Box<Timeline>,
    shrink_message: Option<String>,
}

impl Failure {
    /// The failure category.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// The observed execution record, suitable for [`replay`] or [`Replay::save`].
    /// On a replay mismatch, this contains the steps actually executed. On an
    /// iteration limit, this is the last execution, which passed.
    pub fn replay(&self) -> &Replay {
        self.timeline.replay()
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
        if self.kind == FailureKind::IterationLimit {
            let plural = |count: usize| if count == 1 { "" } else { "s" };
            let (executions, steps) = (self.replay().iteration(), self.replay().steps());
            writeln!(
                f,
                "DFS ran {executions} execution{} and schedules remain. The last execution took {steps} scheduling step{}.",
                plural(executions),
                plural(steps)
            )?;
            return write!(
                f,
                "DFS suits scenarios of a handful of steps. For this scenario, use SchedulerMode::Pct. To finish the search, raise Config::iterations. To stop after {executions} execution{} without failing, use SchedulerMode::DfsPrefix.",
                plural(executions)
            );
        }
        let record = serde_json::to_string(self.replay()).map_err(|_| fmt::Error)?;
        writeln!(f, "{record}")?;
        self.timeline.fmt_events(f)?;
        writeln!(
            f,
            "Rerun the same test using Config::from_env with these environment settings\n  ADAMANTIUM_SEED={} ADAMANTIUM_ITERATIONS={}",
            self.replay().seed(),
            self.replay().iterations()
        )?;
        write!(
            f,
            "For a replay fixture, use Replay::save on failure.replay(), then load it with Replay::load and run testing::replay with the same scenario."
        )
    }
}

impl std::error::Error for Failure {}

/// Explores schedules and returns the last successful record or the first failure.
/// Create fresh state in the factory for each execution. Use modeled primitives
/// and virtual time in place of real I/O and wall-clock waits.
///
/// The executions run on a new thread named like the caller's. It reports to
/// the caller's `tracing` subscriber inside the caller's current span, but the
/// caller's other thread-locals aren't visible to the scenario. [`replay`],
/// [`shrink`], [`explain`] and [`replay_corpus`] run the same way.
///
/// Random, PCT and `DfsPrefix` checks stop at the iteration limit. PCT counts
/// calibration as one execution and stops early if it finds no task choices.
/// A `Dfs` check that reaches the limit with schedules left returns
/// [`FailureKind::IterationLimit`]. Exceeding `max_steps` returns
/// [`FailureKind::StepLimit`]. The limit counts scheduling operations and
/// cannot stop a CPU-only loop.
///
/// # Panics
///
/// Panics for zero bounds, a wall epoch before the Unix epoch, a nested
/// harness, an active Tokio runtime, or Shuttle overrides that change the seed.
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
        wall_epoch,
    } = exploration.config;
    assert!(
        iterations > 0 && max_steps > 0,
        "adamantium::testing bounds must be nonzero"
    );
    assert!(
        wall_epoch >= SystemTime::UNIX_EPOCH,
        "adamantium::testing wall_epoch must not precede UNIX_EPOCH"
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
    let scheduler: Box<dyn Scheduler + Send> = match exploration.scheduler {
        SchedulerMode::Random => Box::new(RandomScheduler::new_from_seed(seed, iterations)),
        SchedulerMode::Pct { depth } => {
            Box::new(PctScheduler::new_from_seed(seed, depth, iterations))
        }
        SchedulerMode::Dfs | SchedulerMode::DfsPrefix => Box::new(DfsScheduler::new(None, true)),
    };
    let (record, _) = run(record, scheduler, None, factory)?;
    if exploration.scheduler == SchedulerMode::Dfs
        && record.dfs_status == Some(DfsStatus::IterationLimit)
    {
        let kind = FailureKind::IterationLimit;
        let failure = Failure {
            kind,
            timeline: Box::new(build_timeline(record, Some(kind), None)),
            shrink_message: None,
        };
        eprintln!("{failure}");
        return Err(failure);
    }
    Ok(record)
}

/// Replays one saved execution with fresh state, the original step limit and
/// the original [`Config::wall_epoch`].
/// Incompatible metadata panics before the scenario runs. The fixture inputs
/// must match the original run. A replay that departs from the record runs
/// to the end of the execution and fails with [`FailureKind::ReplayMismatch`].
pub fn replay<F, Fut>(record: &Replay, factory: F) -> Result<Replay, Failure>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    record.assert_valid();
    play(record, ReplayPolicy::Exact, factory).map(|(record, _)| record)
}

/// Replays `record` with timeline tracing enabled to reconstruct task names,
/// virtual timestamps, modeled operations, waits and wakeups without altering the schedule.
///
/// The returned [`Timeline`] excludes all user payloads and panic messages.
pub fn explain<F, Fut>(record: &Replay, factory: F) -> Timeline
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    record.assert_valid();

    match play(record, ReplayPolicy::Exact, factory) {
        Ok((replay, collector)) => build_timeline(replay, None, collector),
        Err(failure) => *failure.timeline,
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

/// Runs independent test workers and joins their results in input order.
/// The timeline names them `parallel-0`, `parallel-1`, and so on.
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
        .enumerate()
        .map(|(index, worker)| {
            let _scope = trace::enter_operation("testing::parallel");
            shuttle::future::spawn(crate::runtime::coop::model::task(async move {
                trace::record_current_task_name(&format!("parallel-{index}"));
                worker.await
            }))
        })
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
