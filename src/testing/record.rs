use std::{fmt, io, path::Path, time::SystemTime};

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::replay;
use super::{DfsStatus, Exploration, SchedulerMode};

const ENGINE: &str = "shuttle=0.9.10;engine=0.1.8;std=0.1.5;schedulers=0.1.1";
const FORMAT: u32 = 5;

/// A payload-free, versioned execution record. Use [`replay`] to run it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replay {
    format: u32,
    engine: String,
    #[serde(deserialize_with = "deserialize_scheduler")]
    pub(super) scheduler: SchedulerMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) dfs_status: Option<DfsStatus>,
    pub(super) seed: u64,
    pub(super) iterations: usize,
    pub(super) iteration: usize,
    pub(super) max_steps: usize,
    pub(super) wall_epoch: SystemTime,
    pub(super) data_seed: u64,
    pub(super) steps: Vec<Option<usize>>,
}

impl Replay {
    pub(super) fn new(exploration: Exploration) -> Self {
        Self {
            format: FORMAT,
            engine: ENGINE.into(),
            scheduler: exploration.scheduler,
            dfs_status: None,
            seed: exploration.config.seed,
            iterations: exploration.config.iterations,
            iteration: 0,
            max_steps: exploration.config.max_steps,
            wall_epoch: exploration.config.wall_epoch,
            data_seed: 0,
            steps: Vec::new(),
        }
    }

    /// The original exploration scheduler, including any mode-specific bound.
    pub fn scheduler(&self) -> SchedulerMode {
        self.scheduler
    }

    /// Coverage of the original `Dfs` or `DfsPrefix` check. Absent for other
    /// modes and for a DFS check that found a failing schedule.
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

    /// The number of scheduling steps in this execution: each task choice and
    /// each random draw. A DFS tree has one level per task choice.
    pub fn steps(&self) -> usize {
        self.steps.len()
    }

    /// What [`crate::time::wall_now`] reported at the start of the execution.
    pub fn wall_epoch(&self) -> SystemTime {
        self.wall_epoch
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

    pub(super) fn assert_valid(&self) {
        let validation = self.validate();
        assert!(validation.is_ok(), "{validation:?}");
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
        if self.dfs_status.is_some() && !self.scheduler.is_dfs() {
            return Err(invalid_replay("dfs_status requires a DFS scheduler"));
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
