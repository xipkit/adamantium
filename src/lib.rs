//! Adamantium is the fragment of Rust in which every value can cross a task,
//! no lock outlives one synchronous method, and no task outlives its scope.
//!
//! The crate has two halves. The library gives that fragment its vocabulary:
//! copy-on-write values ([`Str`], [`Bytes`], [`Array`], [`Deque`], [`Map`], [`Set`]), a
//! structured task scope (`Tasks`), shared state whose methods run one at
//! a time ([`Actor`]), a bounded channel ([`Channel`]), a one-shot reply
//! ([`Reply`]), the latest value of shared state ([`Watch`]), and `blocking`
//! for code that must block a thread. The `cargo adamantium` subcommand applies
//! the Clippy rules that keep a crate inside the fragment.
//!
//! Everything here is `Send + Sync + 'static` when its element types are, so
//! a value of any of these types can be handed to a task without ceremony.
//!
//! The default `tokio` feature provides `Task`, `Tasks`, `concurrently`, the
//! ordered and unordered concurrent streams, `blocking`, the `time` module, and
//! `retry` and `RateLimit`, whose waits all end when a `Cancellation` is
//! requested.
//! Disable default features to use the values, actors, channels, replies,
//! and watches without a Tokio dependency, including on WASI.
//! The opt-in `signal` feature adds `Cancellation::cancel_on_signal` on Unix
//! and Windows and enables Tokio's process signal support.
//! Enable the optional `tracing` feature for named task spans and warnings
//! identifying unfinished named tasks when a scope's deadline expires.

#![forbid(unsafe_code)]

mod actor;
mod array;
mod bytes;
mod channel;
mod deque;
mod map;
#[cfg(feature = "tokio")]
mod rate_limit;
mod reply;
#[cfg(feature = "tokio")]
mod retry;
mod runtime;
mod set;
mod string;
#[cfg(feature = "tokio")]
mod tasks;
#[cfg(feature = "deterministic")]
pub mod testing;
#[cfg(feature = "tokio")]
pub mod time;
mod watch;

pub use actor::Actor;
pub use array::Array;
pub use bytes::Bytes;
pub use channel::{Channel, Closed, Receiver, Sender, TryReceiveError, TrySendError};
#[cfg(feature = "tokio")]
pub use channel::{ReceiveError, SendError};
pub use deque::Deque;
pub use map::{Entry, Map, OccupiedEntry, VacantEntry};
#[cfg(feature = "tokio")]
pub use rate_limit::RateLimit;
pub use reply::{Reply, ReplyReceiver};
#[cfg(feature = "tokio")]
pub use retry::{Backoff, retry, retry_if};
pub use set::Set;
pub use string::Str;
#[cfg(feature = "tokio")]
pub use tasks::{
    Cancellation, ScopeError, Task, Tasks, blocking, concurrently, concurrently_ordered,
    concurrently_ordered_until, concurrently_unordered, concurrently_unordered_until,
};
pub use watch::{Watch, Watcher};

// Runs the migration guide's examples as doctests, so its rewrites keep
// compiling against the current API.
#[cfg(all(doctest, feature = "tokio"))]
#[doc = include_str!("../docs/migrating.md")]
struct MigratingGuide;
