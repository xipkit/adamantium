mod check;
mod config;
mod deps;
mod fix;
mod lints;
mod manifest;
mod sarif;
mod style;

use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};

use crate::lints::{OVERFLOW_EXAMPLE, OVERFLOW_RULE, OVERFLOW_WHY, lints_toml};

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
        /// Also run the experimental lints from this explicitly built Dylint library.
        #[arg(long)]
        dylint_library: Option<PathBuf>,
        /// Include the strict rules for arithmetic and synchronization.
        #[arg(long)]
        strict: bool,
        /// Leave out the findings in an earlier `--format json` report, so only
        /// new findings fail the check.
        #[arg(long, value_name = "REPORT")]
        baseline: Option<PathBuf>,
        /// Arguments passed on to `cargo clippy`, such as `--release` or `-p name`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    /// Apply machine-applicable fixes and mechanical migration rewrites.
    Fix {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print a unified diff without changing source files.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        strict: bool,
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
        /// Include the strict rules for arithmetic and synchronization.
        #[arg(long)]
        strict: bool,
    },
    /// Report which dependencies use unsafe code or call the paths that
    /// clippy.toml disallows, such as task spawns.
    Deps {
        /// The project directory, or its Cargo.toml.
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = deps::Format::Text)]
        format: deps::Format,
        /// Fail on new unsafe code or unscoped task starts, or on scan gaps.
        #[arg(long)]
        deny_new: bool,
        /// Compare against an earlier `deps --format json` report.
        #[arg(long, value_name = "REPORT", requires = "deny_new")]
        baseline: Option<PathBuf>,
        /// Arguments passed on to `cargo metadata` and `cargo tree`, such as `--features` or
        /// `--all-features`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    /// Print the rules.
    Rules {
        #[arg(long, value_enum, default_value_t = RulesFormat::Text)]
        format: RulesFormat,
        /// Include the strict rules for arithmetic and synchronization.
        #[arg(long)]
        strict: bool,
    },
    /// Explain a rule with a before-and-after example and its replacement.
    Explain {
        /// A bare or qualified rule name, such as unwrap_used or clippy::unwrap_used.
        rule: String,
        #[arg(long, value_enum, default_value_t = ExplainFormat::Text)]
        format: ExplainFormat,
    },
    /// Review Rust files against the judgment-based parts of the Google Rust
    /// style guide, through a model provider (the OpenAI Decisions API by
    /// default).
    #[command(subcommand)]
    Style(style::Command),
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    /// Clippy's own output, then a summary, on stderr.
    Text,
    /// A report on stdout.
    Json,
    /// A SARIF 2.1.0 log on stdout, for GitHub code scanning.
    Sarif,
}

#[derive(Clone, Copy, ValueEnum)]
enum RulesFormat {
    Text,
    Json,
    /// The `[lints]` tables as `cargo adamantium init` writes them into a workspace.
    Toml,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExplainFormat {
    Text,
    Json,
}

#[derive(serde::Serialize)]
struct Explanation {
    schema_version: u32,
    rule: String,
    why: &'static str,
    #[serde(flatten)]
    example: lints::Example,
}

impl Explanation {
    fn for_rule(rule: &str) -> Result<Self> {
        if rule == OVERFLOW_RULE || Some(rule) == OVERFLOW_RULE.strip_prefix("adamantium::") {
            return Ok(Self {
                schema_version: 1,
                rule: OVERFLOW_RULE.to_owned(),
                why: OVERFLOW_WHY,
                example: OVERFLOW_EXAMPLE,
            });
        }
        let lint = lints::selected(true)
            .find(|lint| lint.name == rule || lint.qualified() == rule)
            .ok_or_else(|| {
                anyhow!("unknown rule `{rule}`; `cargo adamantium rules` lists the lint names")
            })?;
        Ok(Self {
            schema_version: 1,
            rule: lint.qualified(),
            why: lint.why,
            example: lint.example,
        })
    }
}

#[derive(serde::Serialize)]
struct Rules<'a> {
    schema_version: u32,
    lints: Vec<&'static lints::Lint>,
    clippy_toml: &'a str,
}

fn main() -> ExitCode {
    // Cargo runs `cargo-adamantium adamantium ...`; drop the repeated name.
    let mut args = std::env::args_os().peekable();
    let program = args.next();
    if args.peek().is_some_and(|arg| arg == "adamantium") {
        args.next();
    }
    match run(Cli::parse_from(program.into_iter().chain(args))) {
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
            strict,
            baseline,
            cargo_args,
            dylint_library,
        } => check::run(
            &path,
            &cargo_args,
            format,
            strict,
            baseline.as_deref(),
            dylint_library.as_deref(),
        ),
        Command::Fix {
            path,
            dry_run,
            strict,
            cargo_args,
        } => fix::run(&path, &cargo_args, strict, dry_run),
        Command::Init {
            path,
            force,
            strict,
        } => {
            let root = manifest::root(&path)?;
            let config = config::Config::load(&root)?;
            let changes = manifest::init(&root, force, &config, strict)?;
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
        Command::Deps {
            path,
            format,
            deny_new,
            baseline,
            cargo_args,
        } => deps::run(&path, format, &cargo_args, deny_new, baseline.as_deref()),
        Command::Style(command) => style::run(command),
        Command::Explain { rule, format } => {
            let explanation = Explanation::for_rule(&rule)?;
            let mut stdout = std::io::stdout().lock();
            match format {
                ExplainFormat::Text => {
                    writeln!(stdout, "{}\n\nWhy: {}", explanation.rule, explanation.why)?;
                    writeln!(stdout, "\nReplacement: {}", explanation.example.replacement)?;
                    writeln!(stdout, "\nBefore:\n{}", explanation.example.before)?;
                    writeln!(stdout, "\nAfter:\n{}", explanation.example.after)?;
                }
                ExplainFormat::Json => {
                    serde_json::to_writer_pretty(&mut stdout, &explanation)?;
                    writeln!(stdout)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Rules { format, strict } => {
            let mut stdout = std::io::stdout().lock();
            match format {
                RulesFormat::Text => {
                    for lint in lints::selected(strict) {
                        writeln!(
                            stdout,
                            "{:<6} {:<36} {}",
                            lint.level.to_string(),
                            lint.qualified(),
                            lint.why
                        )?;
                    }
                    writeln!(stdout, "\n# clippy.toml\n\n{}", lints::clippy_toml(strict)?)?;
                }
                RulesFormat::Json => {
                    serde_json::to_writer_pretty(
                        &mut stdout,
                        &Rules {
                            schema_version: 1,
                            lints: lints::selected(strict).collect(),
                            clippy_toml: &lints::clippy_toml(strict)?,
                        },
                    )?;
                    writeln!(stdout)?;
                }
                RulesFormat::Toml => write!(stdout, "{}", lints_toml(true, strict))?,
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
