//! Clippy only resolves a configured path in a crate the checked crate uses.

#[cfg(feature = "async-channel")]
use async_channel as _;
#[cfg(feature = "async-std")]
use async_std as _;
#[cfg(feature = "crossbeam-channel")]
use crossbeam_channel as _;
#[cfg(feature = "fastrand")]
use fastrand as _;
#[cfg(feature = "flume")]
use flume as _;
#[cfg(feature = "futures")]
use futures as _;
#[cfg(feature = "parking_lot")]
use parking_lot as _;
#[cfg(feature = "rand")]
use rand as _;
#[cfg(feature = "rand_08")]
use rand_08 as _;
#[cfg(feature = "rand_09")]
use rand_09 as _;
#[cfg(feature = "smol")]
use smol as _;
#[cfg(feature = "tokio")]
use tokio as _;
