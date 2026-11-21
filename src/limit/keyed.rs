use std::{collections::HashMap, future::Future, hash::Hash, sync::Arc};

use super::{Kind, Limit, Place};
use crate::runtime::Lock;

#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Runs at most one call per key. Clones share the same set of keys.
///
/// Different keys can run concurrently. Waiting calls have no guaranteed
/// order. A key is removed when its last running or waiting call ends.
/// Work may run on its own or inside a [`Limit`]. While holding a key,
/// it cannot enter any [`Limit`] or [`KeyedLimit`]. These checks apply
/// to direct calls. Avoid waiting on a spawned task that needs a held place.
///
/// ```
/// use adamantium::{KeyedLimit, Limit};
/// # async fn refresh_if_stale(session: &str) {}
/// # async fn example() {
/// let connections = Limit::new(3);
/// let sessions = KeyedLimit::new();
/// let session = "session-a";
/// connections.run(|| sessions.run(session, || refresh_if_stale(session))).await;
/// # }
/// ```
pub struct KeyedLimit<K> {
    keys: Arc<Lock<HashMap<K, Entry>>>,
}

struct Entry {
    limit: Limit,
    calls: usize,
}

struct Registration<'a, K: Eq + Hash> {
    table: &'a KeyedLimit<K>,
    key: K,
}

impl<K> KeyedLimit<K> {
    /// Creates an empty set of independent, single-place limits.
    pub fn new() -> Self {
        Self {
            keys: Arc::new(Lock::new(HashMap::new())),
        }
    }
}

impl<K> Default for KeyedLimit<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> Clone for KeyedLimit<K> {
    fn clone(&self) -> Self {
        Self {
            keys: Arc::clone(&self.keys),
        }
    }
}

impl<K: Clone + Eq + Hash> KeyedLimit<K> {
    /// Waits for exclusive use of `key`, then invokes `work`. Dropping the
    /// future gives up the wait or releases the key.
    ///
    /// # Panics
    ///
    /// Panics when polled inside work holding any [`KeyedLimit`].
    #[allow(clippy::future_not_send, reason = "Send follows the key and work")]
    pub async fn run<Fut: Future>(&self, key: K, work: impl FnOnce() -> Fut) -> Fut::Output {
        Kind::Keyed.run(self.take(key), work).await
    }

    /// Stops waiting or running work when cancellation is requested or the
    /// deadline passes. Cancellation, deadlines, and work errors follow
    /// the same rules as [`Limit::run_until`].
    ///
    /// # Panics
    ///
    /// Panics when polled inside work holding any [`KeyedLimit`].
    #[cfg(feature = "tokio")]
    #[allow(clippy::future_not_send, reason = "Send follows the key")]
    pub async fn run_until<Fut, T, E>(
        &self,
        key: K,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        work: impl FnOnce() -> Fut + Send,
    ) -> Result<T, ScopeError<E>>
    where
        Fut: Future<Output = Result<T, E>> + Send,
    {
        Kind::Keyed
            .run_until(self.take(key), cancellation, deadline, work)
            .await
    }

    #[allow(clippy::future_not_send, reason = "Send follows the key")]
    async fn take(&self, key: K) -> (Place, Registration<'_, K>) {
        let limit = {
            let mut keys = self.keys.lock("KeyedLimit::take");
            let entry = keys.entry(key.clone()).or_insert_with(|| Entry {
                limit: Limit::new(1),
                calls: 0,
            });
            entry.calls += 1;
            entry.limit.clone()
        };
        let registration = Registration { table: self, key };
        let place = limit.take().await;
        (place, registration)
    }
}

impl<K: Eq + Hash> Drop for Registration<'_, K> {
    fn drop(&mut self) {
        // Register and evict under one lock so a key never has two live limits.
        let mut keys = self.table.keys.lock("Registration::drop");
        if let Some(entry) = keys.get_mut(&self.key) {
            entry.calls -= 1;
            if entry.calls == 0 {
                keys.remove(&self.key);
            }
        }
    }
}
