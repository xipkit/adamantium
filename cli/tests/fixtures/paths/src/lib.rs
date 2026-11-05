//! Clippy only resolves a configured path in a crate the checked crate uses.

#[cfg(feature = "futures")]
use futures as _;
#[cfg(feature = "parking_lot")]
use parking_lot as _;
#[cfg(feature = "tokio")]
use tokio as _;
