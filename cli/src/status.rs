use std::process::ExitCode;

use serde::{Deserialize, Serialize};

/// The outcome of a command that looks for findings, and its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Clean,
    Findings,
    Error,
}

impl Status {
    /// The exit status: 0 when clean, 1 with findings and 2 on an error.
    pub(crate) fn exit_code(self) -> ExitCode {
        match self {
            Status::Clean => ExitCode::SUCCESS,
            Status::Findings => ExitCode::from(1),
            Status::Error => ExitCode::from(2),
        }
    }
}
