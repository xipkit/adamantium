#![forbid(unsafe_code)]

mod actor;
mod array;
#[cfg(feature = "tokio")]
mod async_actor;
mod bounded_map;
mod bytes;
mod channel;
mod deque;
mod fanout;
mod limit;
mod map;
mod race;
pub mod random;
#[cfg(feature = "tokio")]
mod rate_limit;
mod reply;
#[cfg(feature = "tokio")]
mod restarts;
#[cfg(feature = "tokio")]
mod retry;
mod runtime;
mod set;
mod single_flight;
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
pub use bounded_map::{BoundedMap, BoundedMapIter, Inserted};
pub use bytes::Bytes;
pub use channel::{Channel, Closed, Receiver, Sender, TryReceiveError, TrySendError};
#[cfg(feature = "tokio")]
pub use channel::{ReceiveError, SendError};
pub use deque::Deque;
#[cfg(feature = "tokio")]
pub use fanout::FeedError;
pub use fanout::{Ended, Fanout, Feed, Sent};
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
pub use single_flight::SingleFlight;
pub use string::Str;
#[cfg(feature = "tokio")]
pub use tasks::{
    Cancellation, ScopeError, SpawnError, Spawner, Task, Tasks, blocking, concurrently,
    concurrently_ordered, concurrently_ordered_until, concurrently_unordered,
    concurrently_unordered_until, first_ok,
};
pub use topic::{Subscriptions, Topic};
pub use watch::{Watch, Watcher};

#[cfg(all(doctest, feature = "tokio"))]
#[doc = include_str!("../docs/migrating.md")]
struct MigratingGuide;
