use std::{
    ffi::OsStr,
    io,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::{FailureKind, check};

/// Bounds for schedule exploration.
///
/// `seed` controls random and PCT scheduling. DFS uses a fixed data seed.
/// `iterations` and `max_steps` must be nonzero.
/// `wall_epoch` sets the initial [`crate::time::wall_now`] for each execution.
/// It defaults to 2026-01-01T00:00:00Z and must be at or after the Unix epoch.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub seed: u64,
    pub iterations: usize,
    pub max_steps: usize,
    pub wall_epoch: SystemTime,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: 0,
            iterations: 100,
            max_steps: 10_000,
            wall_epoch: SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600),
        }
    }
}

impl Config {
    /// Starts with the defaults and applies any environment overrides.
    ///
    /// `ADAMANTIUM_SEED` sets the unsigned 64-bit seed.
    /// `ADAMANTIUM_ITERATIONS` sets the positive execution limit.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error naming the variable if a supplied value
    /// is non-Unicode, empty, malformed, or out of range.
    pub fn from_env() -> io::Result<Self> {
        Self::with_env_values(
            std::env::var_os("ADAMANTIUM_SEED").as_deref(),
            std::env::var_os("ADAMANTIUM_ITERATIONS").as_deref(),
        )
    }

    fn with_env_values(seed: Option<&OsStr>, iterations: Option<&OsStr>) -> io::Result<Self> {
        let mut config = Self::default();
        if let Some(value) = seed {
            config.seed = parse_env(value, "ADAMANTIUM_SEED must be an unsigned 64-bit integer")?;
        }
        if let Some(value) = iterations {
            config.iterations = parse_env::<std::num::NonZeroUsize>(
                value,
                "ADAMANTIUM_ITERATIONS must be a positive integer that fits in usize",
            )?
            .get();
        }
        Ok(config)
    }

    /// Selects the scheduler for [`check`]. Passing a `Config` directly uses random scheduling.
    pub fn with_scheduler(self, scheduler: SchedulerMode) -> Exploration {
        Exploration {
            config: self,
            scheduler,
        }
    }
}

fn parse_env<T: std::str::FromStr>(value: &OsStr, message: &'static str) -> io::Result<T> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// The scheduler and its exploration bounds, configured with [`Config::with_scheduler`].
#[derive(Clone, Copy, Debug)]
pub struct Exploration {
    pub(super) config: Config,
    pub(super) scheduler: SchedulerMode,
}

impl From<Config> for Exploration {
    fn from(config: Config) -> Self {
        config.with_scheduler(SchedulerMode::Random)
    }
}

/// How [`check`] chooses schedules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerMode {
    /// Samples task choices using the seed.
    Random,
    /// Samples task priorities using the seed, with at most `depth - 1`
    /// priority changes in each execution after calibration.
    Pct { depth: usize },
    /// Enumerates every reachable schedule depth-first. Fails with
    /// [`FailureKind::IterationLimit`] if schedules remain after
    /// `iterations` executions.
    Dfs,
    /// Enumerates schedules in the same order as `Dfs`, but passes after
    /// `iterations` executions even when schedules remain.
    DfsPrefix,
}

impl SchedulerMode {
    pub(super) fn is_dfs(self) -> bool {
        matches!(self, Self::Dfs | Self::DfsPrefix)
    }
}

/// How much of the schedule tree a DFS check covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DfsStatus {
    /// Every reachable schedule ran.
    Complete,
    /// Schedules remained when the check reached its iteration limit.
    IterationLimit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_keep_unspecified_defaults() -> io::Result<()> {
        let defaults = Config::default();
        for (seed, iterations, expected_seed, expected_iterations) in [
            (None, None, defaults.seed, defaults.iterations),
            (Some("42"), None, 42, defaults.iterations),
            (None, Some("10000"), defaults.seed, 10_000),
            (Some("18446744073709551615"), Some("1"), u64::MAX, 1),
        ] {
            let config = Config::with_env_values(seed.map(OsStr::new), iterations.map(OsStr::new))?;
            assert_eq!(config.seed, expected_seed);
            assert_eq!(config.iterations, expected_iterations);
            assert_eq!(config.max_steps, defaults.max_steps);
        }
        Ok(())
    }

    #[test]
    fn env_overrides_reject_invalid_values() {
        let invalid = |result: io::Result<Config>, variable: &str| {
            assert!(
                matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidInput
                    && error.to_string().contains(variable))
            );
        };
        for seed in ["", "-1", "18446744073709551616"] {
            invalid(
                Config::with_env_values(Some(OsStr::new(seed)), None),
                "ADAMANTIUM_SEED",
            );
        }
        for iterations in ["0", "ten", "18446744073709551616"] {
            invalid(
                Config::with_env_values(None, Some(OsStr::new(iterations))),
                "ADAMANTIUM_ITERATIONS",
            );
        }
    }
}
