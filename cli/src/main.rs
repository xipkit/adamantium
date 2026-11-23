mod check;
mod config;
mod deep;
mod deps;
mod fix;
mod lints;
mod manifest;
mod sarif;
mod status;
mod style;

use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use sha2::{Digest, Sha256};

use crate::lints::{OVERFLOW_EXAMPLE, OVERFLOW_RULE, OVERFLOW_WHY, lints_toml};

/// The lowercase hexadecimal SHA-256 digest of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

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
    #[command(about = "Run Clippy with adamantium's rules on a Cargo project")]
    Check(check::Args),
    #[command(about = "Apply machine-applicable fixes and mechanical migration rewrites")]
    Fix {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, help = "Print a unified diff without changing source files")]
        dry_run: bool,
        #[arg(long)]
        strict: bool,
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    #[command(about = "Write the rules into a project, so that plain `cargo clippy` applies them")]
    Init {
        #[arg(default_value = ".", help = "The project directory, or its Cargo.toml")]
        path: PathBuf,
        #[arg(long, help = "Replace a clippy.toml that differs from adamantium's")]
        force: bool,
        #[arg(
            long,
            help = "Include the strict rules for arithmetic and synchronization"
        )]
        strict: bool,
    },
    #[command(
        about = "Report which dependencies use unsafe code or call the paths that clippy.toml disallows, such as task spawns"
    )]
    Deps {
        #[arg(default_value = ".", help = "The project directory, or its Cargo.toml")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = deps::Format::Text)]
        format: deps::Format,
        #[arg(
            long,
            help = "Fail on new unsafe code or unscoped task starts, or on scan gaps"
        )]
        deny_new: bool,
        #[arg(
            long,
            value_name = "REPORT",
            requires = "deny_new",
            help = "Compare against an earlier `deps --format json` report"
        )]
        baseline: Option<PathBuf>,
        #[arg(
            last = true,
            help = "Arguments passed on to `cargo metadata` and `cargo tree`, such as `--features` or `--all-features`"
        )]
        cargo_args: Vec<String>,
    },
    #[command(about = "Print the rules")]
    Rules {
        #[arg(long, value_enum, default_value_t = RulesFormat::Text)]
        format: RulesFormat,
        #[arg(
            long,
            help = "Include the strict rules for arithmetic and synchronization"
        )]
        strict: bool,
    },
    #[command(about = "Explain a rule with a before-and-after example and its replacement")]
    Explain {
        #[arg(help = "A bare or qualified rule name, such as unwrap_used or clippy::unwrap_used")]
        rule: String,
        #[arg(long, value_enum, default_value_t = ExplainFormat::Text)]
        format: ExplainFormat,
    },
    #[command(
        subcommand,
        about = "Review Rust files against the judgment-based parts of the Google Rust style guide, through a model provider (the OpenAI Decisions API by default)"
    )]
    Style(style::Command),
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    #[value(help = "Clippy's own output, then a summary, on stderr")]
    Text,
    #[value(help = "A report on stdout")]
    Json,
    #[value(help = "A SARIF 2.1.0 log on stdout, for GitHub code scanning")]
    Sarif,
}

#[derive(Clone, Copy, ValueEnum)]
enum RulesFormat {
    Text,
    Json,
    #[value(help = "The `[lints]` tables as `cargo adamantium init` writes them into a workspace")]
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
        Command::Check(args) => check::run(args),
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
