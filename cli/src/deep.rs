//! Resolve the native lint library pinned by the installed CLI release.

use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use crate::sha256_hex;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DYLINT: &str = "6.4.0";
const SOURCE_HINT: &str = "use --dylint-library with a library built from your checkout";

#[derive(Deserialize)]
struct Pin {
    version: String,
    host: String,
    nightly: String,
    rustc_commit: String,
    sha256: String,
    size: u64,
}

pub(crate) struct Prepared {
    pub(crate) library: PathBuf,
    pub(crate) runner: PathBuf,
    _directory: tempfile::TempDir,
}

/// Prepare a verified private copy for the lifetime of one compiler run.
pub(crate) fn prepare(root: &Path) -> Result<Prepared> {
    let compiler = output(Command::new("rustc").current_dir(root).arg("-vV"))?;
    let host = field(&compiler, "host: ")?;
    let (platform, suffix, data) = match host {
        "x86_64-unknown-linux-gnu" => {
            ("linux-x86_64", "so", include_str!("deep/linux-x86_64.json"))
        }
        "aarch64-apple-darwin" => (
            "darwin-arm64",
            "dylib",
            include_str!("deep/darwin-arm64.json"),
        ),
        _ => bail!("no prebuilt lint library for compiler host {host}; {SOURCE_HINT}"),
    };
    let pin: Pin = serde_json::from_str::<Option<Pin>>(data)?
        .with_context(|| format!("this CLI has no embedded release checksum for {host}; install a published version or {SOURCE_HINT}"))?;
    ensure!(
        pin.host == host && pin.version == VERSION,
        "lint library pin does not match CLI {VERSION} and host {host}"
    );

    let asset = format!("adamantium-lints-{platform}.{suffix}");
    let cargo_home = env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".cargo")))
        .context("locating Cargo home")?;
    let cache = cargo_home.join("adamantium").join(VERSION).join(&asset);
    let url = format!("https://github.com/xipkit/adamantium/releases/download/v{VERSION}/{asset}");
    let bytes = cached_library(&cache, &pin, || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        Ok(agent
            .get(&url)
            .call()
            .with_context(|| format!("downloading {url}"))?
            .into_body()
            .into_reader())
    })?;
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir()?;
    let toolchain = format!("{}-{host}", pin.nightly);
    let library = directory
        .path()
        .join(format!("libadamantium_lints@{toolchain}.{suffix}"));
    fs::write(&library, bytes)?;
    prepare_compiler(&toolchain, &pin)?;
    let runner = prepare_runner(&cargo_home, directory.path())?;
    Ok(Prepared {
        library,
        runner,
        _directory: directory,
    })
}

fn field<'a>(compiler: &'a str, prefix: &str) -> Result<&'a str> {
    compiler
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .with_context(|| format!("rustc -vV omitted {prefix}"))
}

fn output(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("running {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn prepare_compiler(toolchain: &str, pin: &Pin) -> Result<()> {
    let components = output(
        Command::new("rustup")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .args(["component", "list", "--installed", "--toolchain", toolchain]),
    );
    let ready = components.is_ok_and(|list| {
        ["rustc-dev-", "llvm-tools-"].iter().all(|component| {
            list.lines()
                .any(|line| line == format!("{component}{}", pin.host))
        })
    });
    if !ready {
        let status = Command::new("rustup")
            .args([
                "toolchain",
                "install",
                toolchain,
                "--profile",
                "minimal",
                "--component",
                "rustc-dev,llvm-tools-preview",
                "--no-self-update",
            ])
            .stdout(Stdio::null())
            .status()
            .context("--deep requires rustup to install the pinned nightly")?;
        ensure!(status.success(), "installing {toolchain} failed");
    }
    let compiler = output(Command::new("rustup").args(["run", toolchain, "rustc", "-vV"]))?;
    ensure!(
        field(&compiler, "host: ")? == pin.host
            && field(&compiler, "commit-hash: ")? == pin.rustc_commit,
        "installed compiler does not match the released lint library ({toolchain} at {})",
        pin.rustc_commit
    );
    Ok(())
}

fn prepare_runner(cargo_home: &Path, scratch: &Path) -> Result<PathBuf> {
    let root = std::path::absolute(cargo_home)?
        .join("adamantium-tools")
        .join(DYLINT);
    let installed = root.join("bin/cargo-dylint");
    for runner in [Path::new("cargo-dylint"), installed.as_path()] {
        if output(Command::new(runner).args(["dylint", "--version"]))
            .is_ok_and(|v| v.trim() == format!("cargo-dylint {DYLINT}"))
        {
            return Ok(runner.to_owned());
        }
    }
    let result = Command::new("cargo")
        .current_dir(scratch)
        .args([
            "install",
            "--locked",
            "--registry",
            "crates-io",
            "--version",
            DYLINT,
            "--root",
        ])
        .arg(root)
        .arg("cargo-dylint")
        .stdout(Stdio::null())
        .status()?;
    ensure!(result.success(), "installing cargo-dylint {DYLINT} failed");
    Ok(installed)
}

fn cached_library<R: Read>(
    path: &Path,
    pin: &Pin,
    download: impl FnOnce() -> Result<R>,
) -> Result<Vec<u8>> {
    match fs::File::open(path) {
        Ok(file) => {
            return verified(file, pin).with_context(|| {
                format!(
                    "checking cached {}; remove it to download again",
                    path.display()
                )
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let bytes = verified(download()?, pin)?;
    let parent = path.parent().context("invalid cache path")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&bytes)?;
    temp.persist(path)?;
    Ok(bytes)
}

fn verified(reader: impl Read, pin: &Pin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(pin.size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == pin.size && sha256_hex(&bytes) == pin.sha256,
        "lint library size or SHA-256 differs from the checksum embedded in cargo-adamantium {VERSION}"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_downloads_enter_the_cache() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("release").join("library");
        let pin = Pin {
            version: VERSION.into(),
            host: "test-host".into(),
            nightly: "test-nightly".into(),
            rustc_commit: "test-commit".into(),
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };

        for damaged in [b"ab".as_slice(), b"abd", b"abcd"] {
            assert!(cached_library(&path, &pin, || Ok(damaged)).is_err());
            assert!(!path.exists());
        }
        assert_eq!(
            cached_library(&path, &pin, || Ok(b"abc".as_slice()))?,
            b"abc"
        );
        let offline = || -> Result<&[u8]> { bail!("unexpected download") };
        assert_eq!(cached_library(&path, &pin, offline)?, b"abc");
        fs::write(&path, b"abc-extra")?;
        let error = cached_library(&path, &pin, offline).unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256"));
        Ok(())
    }
}
