mod check;
mod lints;
mod manifest;

use std::{ffi::OsString, io::Write, path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::lints::{CLIPPY_TOML, LINTS, lints_toml};

#[derive(Parser)]
#[command(
    name = "cargo adamantium",
    bin_name = "cargo adamantium",
    version,
    about = "Keeps a crate inside the adamantium fragment of Rust"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run Clippy with adamantium's rules on a Cargo project.
    Check {
        /// The project directory, or its Cargo.toml.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
        format: ReportFormat,
        /// Arguments passed on to `cargo clippy`, such as `--release` or `-p name`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    /// Write the rules into a project, so that plain `cargo clippy` applies them.
    Init {
        /// The project directory, or its Cargo.toml.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Replace a clippy.toml that differs from adamantium's.
        #[arg(long)]
        force: bool,
    },
    /// Print the rules.
    Rules {
        #[arg(long, value_enum, default_value_t = RulesFormat::Text)]
        format: RulesFormat,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    /// Clippy's own output, then a summary, on stderr.
    Text,
    /// A report on stdout.
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum RulesFormat {
    Text,
    Json,
    /// The `[lints]` tables as `cargo adamantium init` writes them into a workspace.
    Toml,
}

#[derive(serde::Serialize)]
struct Rules {
    schema_version: u32,
    lints: &'static [lints::Lint],
    clippy_toml: &'static str,
}

fn main() -> ExitCode {
    // Cargo runs `cargo-adamantium adamantium ...`; drop the repeated name.
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "adamantium") {
        args.remove(1);
    }
    match run(Cli::parse_from(args)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Check {
            path,
            format,
            cargo_args,
        } => check::run(&path, &cargo_args, matches!(format, ReportFormat::Json)),
        Command::Init { path, force } => {
            let root = manifest::root(&path)?;
            let changes = manifest::init(&root, force)?;
            let mut stderr = std::io::stderr().lock();
            if changes.notes.is_empty() {
                writeln!(
                    stderr,
                    "adamantium: {} already has the rules",
                    root.display()
                )?;
            }
            for note in changes.notes {
                writeln!(stderr, "adamantium: {note}")?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Rules { format } => {
            let mut stdout = std::io::stdout().lock();
            match format {
                RulesFormat::Text => {
                    for lint in LINTS {
                        writeln!(
                            stdout,
                            "{:<6} {:<36} {}",
                            lint.level.to_string(),
                            lint.qualified(),
                            lint.why
                        )?;
                    }
                    writeln!(stdout, "\n# clippy.toml\n\n{CLIPPY_TOML}")?;
                }
                RulesFormat::Json => {
                    serde_json::to_writer_pretty(
                        &mut stdout,
                        &Rules {
                            schema_version: 1,
                            lints: LINTS,
                            clippy_toml: CLIPPY_TOML,
                        },
                    )?;
                    writeln!(stdout)?;
                }
                RulesFormat::Toml => write!(stdout, "{}", lints_toml(true))?,
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
