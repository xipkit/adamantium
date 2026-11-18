//! Adamantium is the fragment of Rust in which every value can cross a task,
//! no lock outlives one synchronous method, and no task outlives its scope.
//!
//! The crate has two halves. The library gives that fragment its vocabulary:
//! copy-on-write values ([`Str`], [`Bytes`], [`Array`], [`Deque`], [`Map`], [`Set`]), a
//! structured task scope (`Tasks`), shared state whose methods run one at
//! a time ([`Actor`]), a bounded channel ([`Channel`]), a channel that sends
//! each message to every subscriber ([`Topic`]), a one-shot reply
//! ([`Reply`]), the latest value of shared state ([`Watch`]), [`race`] to wait
//! for the first of several events, a limit on how many calls run at once
//! ([`Limit`]), and `blocking` for code that must block a thread. The
//! `cargo adamantium` subcommand applies the Clippy rules that keep a crate
//! inside the fragment.
//!
//! Everything here is `Send + Sync + 'static` when its element types are, so
//! a value of any of these types can be handed to a task without ceremony.
//!
//! The default `tokio` feature provides `Task`, `Tasks`, `concurrently`,
//! `first_ok`, `AsyncActor`, the ordered and unordered concurrent streams, `blocking`, the
//! `time` module, and `retry` and `RateLimit`, whose waits all end when a
//! `Cancellation` is requested. `Tasks::spawn_supervised` restarts a task
//! that fails, as often as a [`Restarts`] allows.
//! Disable default features to use the values, synchronous actors, channels, replies,
//! and watches without a Tokio dependency.
//!
//! Both configurations build and are tested on `wasm32-wasip1`. WASI has one
//! thread and aborts on a panic, so `blocking` and `Tasks::spawn_blocking` run
//! their closure on that thread while other tasks wait, and a panic ends the
//! program where other targets would report it to the scope.
//! The opt-in `signal` feature adds `Cancellation::cancel_on_signal` on Unix
//! and Windows and enables Tokio's process signal support.
//! Enable the optional `tracing` feature for named task spans and warnings
//! identifying unfinished named tasks when a scope's deadline expires.

#![forbid(unsafe_code)]

mod actor;
mod array;
#[cfg(feature = "tokio")]
mod async_actor;
mod bytes;
mod channel;
mod deque;
mod limit;
mod map;
mod race;
#[cfg(feature = "tokio")]
mod rate_limit;
mod reply;
#[cfg(feature = "tokio")]
mod restarts;
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
mod topic;
mod watch;

pub use actor::Actor;
pub use array::Array;
#[cfg(feature = "tokio")]
pub use async_actor::AsyncActor;
pub use bytes::Bytes;
pub use channel::{Channel, Closed, Receiver, Sender, TryReceiveError, TrySendError};
#[cfg(feature = "tokio")]
pub use channel::{ReceiveError, SendError};
pub use deque::Deque;
pub use limit::{KeyedLimit, Limit};
pub use map::{Entry, Map, OccupiedEntry, VacantEntry};
pub use race::{Race, race};
#[cfg(feature = "tokio")]
pub use race::{UntilError, until};
#[cfg(feature = "tokio")]
pub use rate_limit::RateLimit;
pub use reply::{Reply, ReplyReceiver};
#[cfg(feature = "tokio")]
pub use restarts::Restarts;
#[cfg(feature = "tokio")]
pub use retry::{Backoff, retry, retry_if};
pub use set::Set;
pub use string::Str;
#[cfg(feature = "tokio")]
pub use tasks::{
    Cancellation, ScopeError, SpawnError, Spawner, Task, Tasks, blocking, concurrently,
    concurrently_ordered, concurrently_ordered_until, concurrently_unordered,
    concurrently_unordered_until, first_ok,
};
pub use topic::{Subscriptions, Topic};
pub use watch::{Watch, Watcher};

// Runs the migration guide's examples as doctests, so its rewrites keep
// compiling against the current API.
#[cfg(all(doctest, feature = "tokio"))]
#[doc = include_str!("../docs/migrating.md")]
struct MigratingGuide;
