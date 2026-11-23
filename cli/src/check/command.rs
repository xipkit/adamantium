use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use crate::lints::{self, EXPERIMENTAL_LINTS};

pub(super) fn write_clippy_config(
    dir: &Path,
    strict: bool,
    fix: bool,
) -> Result<(PathBuf, std::borrow::Cow<'static, str>)> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let path = dir.join("clippy.toml");
    let mut toml = lints::clippy_toml(strict)?;
    if fix {
        toml = lints::fix_clippy_toml(&toml)?.into();
    }
    fs::write(&path, toml.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, toml))
}

pub(super) fn clippy_command(
    root: &Path,
    conf_dir: &Path,
    cargo_args: &[String],
    strict: bool,
) -> Command {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root)
        .env("CLIPPY_CONF_DIR", conf_dir)
        .args(["clippy", "--message-format=json"]);
    select_everything(&mut command, cargo_args);
    command
        .args(cargo_args)
        .args(["--", "--cap-lints", "warn", "-A", "clippy::all"]);
    for lint in lints::selected(strict) {
        command.args(["-W", &lint.qualified()]);
    }
    command
}

/// Selects every target of every workspace member, unless the arguments for
/// Cargo already choose targets or packages.
fn select_everything(command: &mut Command, cargo_args: &[String]) {
    if !selects_targets(cargo_args) {
        command.arg("--all-targets");
    }
    if !selects_packages(cargo_args) {
        command.arg("--workspace");
    }
}

pub(super) fn dylint_command(
    root: &Path,
    cargo_args: &[String],
    library: &Path,
    runner: Option<&Path>,
) -> Result<Command> {
    let library = library
        .canonicalize()
        .context("resolving the Dylint library; build lints/ first")?;
    anyhow::ensure!(library.is_file(), "Dylint library is not a file");
    let program = runner
        .map(Path::as_os_str)
        .map(ToOwned::to_owned)
        .or_else(|| env::var_os("CARGO"))
        .unwrap_or_else(|| "cargo".into());
    let listed = Command::new(&program)
        .current_dir(root)
        .args(["dylint", "list", "--no-metadata", "--lib-path"])
        .arg(&library)
        .stdin(Stdio::null())
        .output()
        .context("loading the experimental lints; install cargo-dylint 6.4.0 first")?;
    anyhow::ensure!(
        listed.status.success(),
        "loading the experimental lints: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listing = String::from_utf8_lossy(&listed.stdout);
    for lint in EXPERIMENTAL_LINTS {
        anyhow::ensure!(
            listing.split_whitespace().any(|word| word == *lint),
            "Dylint library does not register {lint}"
        );
    }
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .args(["dylint", "--no-metadata", "--lib-path"])
        .arg(library)
        .args(["--", "--message-format=json"]);
    select_everything(&mut command, cargo_args);
    command.args(cargo_args);
    let mut flags = String::from("--cap-lints warn");
    for lint in EXPERIMENTAL_LINTS {
        flags.push_str(" -W ");
        flags.push_str(lint);
    }
    command.env("DYLINT_RUSTFLAGS", flags);
    Ok(command)
}

fn selects_packages(cargo_args: &[String]) -> bool {
    cargo_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-p" | "--package" | "--workspace" | "--all" | "--exclude"
        ) || arg.starts_with("--package=")
            || (arg.starts_with("-p") && arg.len() > 2)
    })
}

fn selects_targets(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--all-targets"
                | "--lib"
                | "--bin"
                | "--bins"
                | "--example"
                | "--examples"
                | "--test"
                | "--tests"
                | "--bench"
                | "--benches"
        ) || ["--bin=", "--example=", "--test=", "--bench="]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
    })
}
