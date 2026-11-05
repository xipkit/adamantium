//! Adamantium is the fragment of Rust in which every value can cross a task,
//! no lock outlives one synchronous method, and no task outlives its scope.
//!
//! The crate has two halves. The library gives that fragment its vocabulary:
//! copy-on-write values ([`Str`], [`Bytes`], [`Array`], [`Deque`], [`Map`], [`Set`]), a
//! structured task scope (`Tasks`), shared state whose methods run one at
//! a time ([`Actor`]), a bounded channel ([`Channel`]), and `blocking` for
//! code that must block a thread. The `cargo adamantium` subcommand applies
//! the Clippy rules that keep a crate inside the fragment.
//!
//! Everything here is `Send + Sync + 'static` when its element types are, so
//! a value of any of these types can be handed to a task without ceremony.
//!
//! The default `tokio` feature provides `Task`, `Tasks`, `concurrently`, and
//! `blocking`. Disable default features to use the values, actors, and channels
//! without a Tokio dependency, including on WASI.

#![forbid(unsafe_code)]

mod actor;
mod array;
mod bytes;
mod channel;
mod deque;
mod map;
mod set;
mod string;
#[cfg(feature = "tokio")]
mod tasks;

pub use actor::Actor;
pub use array::Array;
pub use bytes::Bytes;
pub use channel::{Channel, Closed, Receiver, Sender, TryReceiveError, TrySendError};
pub use deque::Deque;
pub use map::Map;
pub use set::Set;
pub use string::Str;
#[cfg(feature = "tokio")]
pub use tasks::{Task, Tasks, blocking, concurrently};
