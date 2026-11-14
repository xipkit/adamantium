//! Reading a provider's API key from the environment or, on macOS, the
//! login Keychain.

use std::env;

use anyhow::{Result, bail};

/// Reads the key from the environment variable `variable` or, on macOS when
/// it is unset or empty, from the generic password named `keychain_item`.
#[allow(
    clippy::disallowed_methods,
    reason = "the CLI is synchronous; polling the Keychain helper is the only wait"
)]
pub(crate) fn api_key(variable: &str, keychain_item: &str) -> Result<String> {
    if let Ok(value) = env::var(variable) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_owned());
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::{
            io::Read,
            process::Stdio,
            thread,
            time::{Duration, Instant},
        };

        use anyhow::Context;

        let mut child = std::process::Command::new("security")
            .args(["find-generic-password", "-s", keychain_item, "-w"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("could not run the Keychain lookup")?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().context("Keychain lookup failed")? {
                if status.success() {
                    let mut key = String::new();
                    child
                        .stdout
                        .take()
                        .context("missing Keychain output")?
                        .read_to_string(&mut key)?;
                    if !key.trim().is_empty() {
                        return Ok(key.trim().to_owned());
                    }
                }
                break;
            }
            if started.elapsed() >= Duration::from_secs(10) {
                // Report the timeout below even if cleanup fails.
                drop(child.kill());
                drop(child.wait());
                bail!("Keychain lookup timed out; set {variable} instead");
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("set {variable} or make the {keychain_item} Keychain item accessible");
    }
    #[cfg(not(target_os = "macos"))]
    {
        bail!(
            "set {variable} to use this provider (the {keychain_item} Keychain item is read only on macOS)"
        );
    }
}
