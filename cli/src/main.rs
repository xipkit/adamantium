mod check;
mod config;
mod deep;
mod deps;
mod fix;
mod hash;
mod lints;
mod manifest;
mod paths;
mod sarif;
mod status;
mod style;

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    Deps(deps::Args),
    #[command(about = "Print the rules")]
    Rules {
        #[arg(long, value_enum, default_value_t = lints::RulesFormat::Text)]
        format: lints::RulesFormat,
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
        #[arg(long, value_enum, default_value_t = lints::ExplainFormat::Text)]
        format: lints::ExplainFormat,
    },
    #[command(
        subcommand,
        about = "Review Rust files against the judgment-based parts of the Google Rust style guide, through a model provider (the OpenAI Decisions API by default)"
    )]
    Style(style::Command),
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
        } => manifest::run_init(&path, force, strict),
        Command::Deps(args) => deps::run(args),
        Command::Style(command) => style::run(command),
        Command::Explain { rule, format } => lints::explain(&rule, format),
        Command::Rules { format, strict } => lints::print_rules(format, strict),
    }
}
