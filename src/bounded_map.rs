use std::{borrow::Borrow, fmt, hash::Hash, iter::FusedIterator};
#[cfg(feature = "tokio")]
use std::{collections::BTreeMap, time::Duration};

use indexmap::IndexMap;
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::time;

/// A map that evicts the least recently used entry to stay within capacity,
/// with optional per-entry expiry.
///
/// ```
/// use adamantium::{Actor, BoundedMap, Inserted};
///
/// let sessions = Actor::new(BoundedMap::new(2));
/// let evicted = sessions.with(|s| {
///     drop(s.insert("ana", 1));
///     drop(s.insert("bo", 2));
///     // A lookup makes "ana" the most recently used entry...
///     assert_eq!(s.get("ana"), Some(&1));
///     // ...so a third key evicts "bo".
///     s.insert("cy", 3)
/// });
/// assert_eq!(evicted, Inserted::Evicted("bo", 2));
/// assert_eq!(sessions.read(|s| s.len()), 2);
/// ```
///
/// # Eviction policy
///
/// [`Self::get`], [`Self::get_mut`], and [`Self::insert`] mark the entry as
/// most recently used. [`Self::peek`], [`Self::contains_key`], and [`Self::iter`]
/// preserve the order. Inserting a new key into a full map returns the
/// least recently used live entry in [`Inserted::Evicted`]. At zero capacity,
/// every insert returns the entry just offered.
///
/// # Expiry
///
/// With the `tokio` feature, `with_ttl` sets a default time to live for
/// entries inserted with [`Self::insert`], and `insert_with_ttl` sets a time
/// to live for one entry. Expiry uses `adamantium::time::now`, so it follows
/// Tokio's paused clock and the deterministic harness's virtual clock. Time to
/// live is measured from insertion; reads refresh recency without extending
/// expiry.
///
/// Expired entries are never returned or counted by [`Self::get`],
/// [`Self::get_mut`], [`Self::peek`], [`Self::contains_key`], [`Self::iter`],
/// [`Self::len`], [`Self::is_empty`], [`Self::is_full`], [`Self::remove`], or
/// [`Self::pop_lru`]. They are removed lazily on mutable access and before any
/// live entry is evicted on insert. Call `remove_expired` to sweep all expired
/// entries at once.
///
/// # Costs
///
/// Without expiring entries, lookups, inserts, and removals take amortized
/// O(1) time and never read the clock. Expiring entries maintain an expiry
/// index, making inserts and removals O(log n). Storage grows on demand up to
/// capacity and is kept for reuse. Cloning copies every entry.
#[derive(Clone)]
pub struct BoundedMap<K, V> {
    entries: IndexMap<K, Slot<V>>,
    capacity: usize,
    head: Option<usize>,
    tail: Option<usize>,
    #[cfg(feature = "tokio")]
    ttl: Option<Duration>,
    #[cfg(feature = "tokio")]
    expiries: BTreeMap<(Instant, u64), usize>,
    #[cfg(feature = "tokio")]
    next_expiry_id: u64,
}

#[derive(Clone)]
struct Slot<V> {
    value: V,
    newer: Option<usize>,
    older: Option<usize>,
    #[cfg(feature = "tokio")]
    expiry: Option<(Instant, u64)>,
}

#[cfg(feature = "tokio")]
impl<V> Slot<V> {
    fn is_expired(&self) -> bool {
        self.expiry
            .is_some_and(|(expires_at, _)| expires_at <= time::now())
    }

    fn is_expired_at(&self, now: Instant) -> bool {
        self.expiry.is_some_and(|(expires_at, _)| expires_at <= now)
    }
}

#[must_use = "an evicted or replaced value may need closing or logging; use `drop` to discard it"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inserted<K, V> {
    Added,
    Replaced(V),
    Evicted(K, V),
}

impl<K, V> BoundedMap<K, V> {
    /// An empty map that will hold at most `capacity` entries. Nothing is
    /// allocated until the first insert.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: IndexMap::new(),
            capacity,
            head: None,
            tail: None,
            #[cfg(feature = "tokio")]
            ttl: None,
            #[cfg(feature = "tokio")]
            expiries: BTreeMap::new(),
            #[cfg(feature = "tokio")]
            next_expiry_id: 0,
        }
    }

    /// An empty map that holds at most `capacity` entries and expires entries
    /// inserted with [`Self::insert`] after `ttl` on [`crate::time::now`].
    ///
    /// Nothing is allocated until the first insert. Requires the `tokio`
    /// feature.
    ///
    /// ```
    /// use std::time::Duration;
    /// use adamantium::{BoundedMap, Inserted};
    ///
    /// let mut cache = BoundedMap::with_ttl(10, Duration::from_secs(60));
    /// assert_eq!(cache.insert("session", 1), Inserted::Added);
    /// assert_eq!(cache.get("session"), Some(&1));
    /// assert_eq!(cache.remove_expired(), 0);
    /// ```
    #[cfg(feature = "tokio")]
    pub fn with_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            ttl: Some(ttl),
            ..Self::new(capacity)
        }
    }

    /// The default time to live for entries inserted with [`Self::insert`], if
    /// set. Requires the `tokio` feature.
    #[cfg(feature = "tokio")]
    pub fn ttl(&self) -> Option<Duration> {
        self.ttl
    }

    /// The most entries the map holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of live entries.
    pub fn len(&self) -> usize {
        #[cfg(feature = "tokio")]
        if !self.expiries.is_empty() {
            return self.len_at(time::now());
        }
        self.entries.len()
    }

    /// Whether the map has no live entries.
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "tokio")]
        if !self.expiries.is_empty() {
            if self.entries.len() > self.expiries.len() {
                return false;
            }
            return self
                .expiries
                .last_key_value()
                .is_none_or(|(&(expires_at, _), _)| expires_at <= time::now());
        }
        self.entries.is_empty()
    }

    /// Whether the map holds `capacity` live entries, so that inserting a new
    /// key would evict one.
    pub fn is_full(&self) -> bool {
        if self.entries.len() < self.capacity {
            return false;
        }
        if self.capacity == 0 {
            return true;
        }
        #[cfg(feature = "tokio")]
        if let Some((&(expires_at, _), _)) = self.expiries.first_key_value() {
            return expires_at > time::now();
        }
        true
    }

    /// The live entries from most to least recently used, without changing the
    /// order. Reverse it to start from the next entry to be evicted.
    pub fn iter(&self) -> BoundedMapIter<'_, K, V> {
        #[cfg(feature = "tokio")]
        let (now, remaining) = if self.expiries.is_empty() {
            (None, self.entries.len())
        } else {
            let now = time::now();
            (Some(now), self.len_at(now))
        };
        #[cfg(not(feature = "tokio"))]
        let remaining = self.entries.len();
        BoundedMapIter {
            entries: &self.entries,
            front: self.head,
            back: self.tail,
            remaining,
            #[cfg(feature = "tokio")]
            now,
        }
    }

    /// Removes every entry, keeping the capacity and the storage.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.head = None;
        self.tail = None;
        #[cfg(feature = "tokio")]
        self.expiries.clear();
    }

    /// Removes every expired entry and returns the number removed.
    ///
    /// Requires the `tokio` feature.
    #[cfg(feature = "tokio")]
    pub fn remove_expired(&mut self) -> usize {
        if self.expiries.is_empty() {
            return 0;
        }
        let now = time::now();
        let mut removed = 0;
        while self.remove_one_expired(now) {
            removed += 1;
        }
        removed
    }

    /// Removes and returns the least recently used live entry, the one the next
    /// insert of a new key into a full map would evict.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        #[cfg(feature = "tokio")]
        let now = (!self.expiries.is_empty()).then(time::now);
        #[cfg(feature = "tokio")]
        return self.pop_lru_at(now);
        #[cfg(not(feature = "tokio"))]
        {
            let tail = self.tail?;
            self.remove_index(tail)
        }
    }

    #[cfg(feature = "tokio")]
    fn pop_lru_at(&mut self, now: Option<Instant>) -> Option<(K, V)> {
        while let Some(tail) = self.tail {
            let expired =
                now.is_some_and(|now| self.slot(tail).is_some_and(|slot| slot.is_expired_at(now)));
            let entry = self.remove_index(tail)?;
            if !expired {
                return Some(entry);
            }
        }
        None
    }

    #[cfg(feature = "tokio")]
    fn len_at(&self, now: Instant) -> usize {
        let expired = self
            .expiries
            .keys()
            .take_while(|&&(expires_at, _)| expires_at <= now)
            .count();
        self.entries.len().saturating_sub(expired)
    }

    #[cfg(feature = "tokio")]
    fn is_index_expired(&self, index: usize) -> bool {
        self.slot(index).is_some_and(Slot::is_expired)
    }

    #[cfg(feature = "tokio")]
    fn allocate_expiry_key(&mut self, expires_at: Instant) -> (Instant, u64) {
        let mut id = self.next_expiry_id;
        while self.expiries.contains_key(&(expires_at, id)) {
            id = id.wrapping_add(1);
        }
        self.next_expiry_id = id.wrapping_add(1);
        (expires_at, id)
    }

    #[cfg(feature = "tokio")]
    fn remove_one_expired(&mut self, now: Instant) -> bool {
        let Some((&(expires_at, _), &index)) = self.expiries.first_key_value() else {
            return false;
        };
        if expires_at > now {
            return false;
        }
        self.remove_index(index).is_some()
    }

    fn slot(&self, index: usize) -> Option<&Slot<V>> {
        self.entries.get_index(index).map(|(_, slot)| slot)
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut Slot<V>> {
        self.entries.get_index_mut(index).map(|(_, slot)| slot)
    }

    fn unlink(&mut self, index: usize) {
        let Some(&Slot { newer, older, .. }) = self.slot(index) else {
            return;
        };
        match newer.and_then(|newer| self.slot_mut(newer)) {
            Some(slot) => slot.older = older,
            None => self.head = older,
        }
        match older.and_then(|older| self.slot_mut(older)) {
            Some(slot) => slot.newer = newer,
            None => self.tail = newer,
        }
    }

    fn push_front(&mut self, index: usize) {
        let old_head = self.head;
        let Some(slot) = self.slot_mut(index) else {
            return;
        };
        slot.newer = None;
        slot.older = old_head;
        match old_head.and_then(|head| self.slot_mut(head)) {
            Some(slot) => slot.newer = Some(index),
            None => self.tail = Some(index),
        }
        self.head = Some(index);
    }

    fn touch(&mut self, index: usize) {
        if self.head != Some(index) {
            self.unlink(index);
            self.push_front(index);
        }
    }

    fn remove_index(&mut self, index: usize) -> Option<(K, V)> {
        self.unlink(index);
        let (key, slot) = self.entries.swap_remove_index(index)?;
        #[cfg(feature = "tokio")]
        if let Some(expiry) = slot.expiry {
            self.expiries.remove(&expiry);
        }
        if let Some(moved) = self.slot(index) {
            let newer = moved.newer;
            let older = moved.older;
            #[cfg(feature = "tokio")]
            let moved_expiry = moved.expiry;
            match newer.and_then(|newer| self.slot_mut(newer)) {
                Some(slot) => slot.older = Some(index),
                None => self.head = Some(index),
            }
            match older.and_then(|older| self.slot_mut(older)) {
                Some(slot) => slot.newer = Some(index),
                None => self.tail = Some(index),
            }
            #[cfg(feature = "tokio")]
            if let Some(moved_expiry) = moved_expiry
                && let Some(entry) = self.expiries.get_mut(&moved_expiry)
            {
                *entry = index;
            }
        }
        Some((key, slot.value))
    }
}

impl<K: Hash + Eq, V> BoundedMap<K, V> {
    /// The value for `key`, which becomes the most recently used entry.
    ///
    /// Returns `None` if `key` is absent or expired, removing an expired entry.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
        #[cfg(feature = "tokio")]
        if self.is_index_expired(index) {
            self.remove_index(index);
            return None;
        }
        self.touch(index);
        self.slot(index).map(|slot| &slot.value)
    }

    /// Mutable access to the value for `key`, which becomes the most
    /// recently used entry.
    ///
    /// Returns `None` if `key` is absent or expired, removing an expired entry.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
        #[cfg(feature = "tokio")]
        if self.is_index_expired(index) {
            self.remove_index(index);
            return None;
        }
        self.touch(index);
        self.slot_mut(index).map(|slot| &mut slot.value)
    }

    /// The value for `key`, without changing the order of use.
    ///
    /// Returns `None` if `key` is absent or expired.
    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let slot = self.entries.get(key)?;
        #[cfg(feature = "tokio")]
        if slot.is_expired() {
            return None;
        }
        Some(&slot.value)
    }

    /// Whether `key` is present and not expired, without changing the order of
    /// use.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.peek(key).is_some()
    }

    /// Inserts `value` for `key` and makes it the most recently used entry.
    ///
    /// A live existing key keeps its stored key and gets the new value (and a
    /// fresh default time to live, if the map has one). An expired entry for
    /// `key` is removed as if absent. A new key in a full map removes an
    /// expired entry first, if any, before evicting the least recently used
    /// live entry. The result says which of these happened and returns whatever
    /// live entry left the map.
    pub fn insert(&mut self, key: K, value: V) -> Inserted<K, V> {
        #[cfg(feature = "tokio")]
        {
            self.insert_inner(key, value, self.ttl)
        }
        #[cfg(not(feature = "tokio"))]
        {
            self.insert_no_ttl(key, value)
        }
    }

    /// Inserts `value` for `key` with its own time to live `ttl`, overriding
    /// the map's default time to live if one is set, and makes it the most
    /// recently used entry.
    ///
    /// A zero `ttl` expires immediately: at positive capacity it replaces and
    /// removes any live entry for `key`, and otherwise adds nothing without
    /// evicting a live entry. Requires the `tokio` feature.
    #[cfg(feature = "tokio")]
    pub fn insert_with_ttl(&mut self, key: K, value: V, ttl: Duration) -> Inserted<K, V> {
        self.insert_inner(key, value, Some(ttl))
    }

    #[cfg(not(feature = "tokio"))]
    fn insert_no_ttl(&mut self, key: K, value: V) -> Inserted<K, V> {
        if let Some((index, _, slot)) = self.entries.get_full_mut(&key) {
            let old = std::mem::replace(&mut slot.value, value);
            self.touch(index);
            return Inserted::Replaced(old);
        }
        if self.capacity == 0 {
            return Inserted::Evicted(key, value);
        }
        let evicted = if self.is_full() { self.pop_lru() } else { None };
        self.reserve_one();
        let (index, _) = self.entries.insert_full(
            key,
            Slot {
                value,
                newer: None,
                older: None,
            },
        );
        self.push_front(index);
        match evicted {
            Some((key, value)) => Inserted::Evicted(key, value),
            None => Inserted::Added,
        }
    }

    #[cfg(feature = "tokio")]
    fn insert_inner(&mut self, key: K, value: V, ttl: Option<Duration>) -> Inserted<K, V> {
        if self.capacity == 0 {
            return Inserted::Evicted(key, value);
        }
        let now = (ttl.is_some() || !self.expiries.is_empty()).then(time::now);

        if let Some(index) = self.entries.get_index_of(&key) {
            let existing_expired =
                now.is_some_and(|now| self.slot(index).is_some_and(|slot| slot.is_expired_at(now)));
            if existing_expired {
                self.remove_index(index);
            } else {
                if ttl.is_some_and(|ttl| ttl.is_zero()) {
                    return match self.remove_index(index) {
                        Some((_, old)) => Inserted::Replaced(old),
                        None => Inserted::Added,
                    };
                }
                let new_expires_at = ttl.and_then(|ttl| now.and_then(|now| now.checked_add(ttl)));
                let old_expiry = self.slot_mut(index).and_then(|slot| slot.expiry.take());
                if let Some(old_expiry) = old_expiry {
                    self.expiries.remove(&old_expiry);
                }
                let new_expiry = new_expires_at.map(|expires_at| {
                    let expiry = self.allocate_expiry_key(expires_at);
                    self.expiries.insert(expiry, index);
                    expiry
                });
                let old = self.slot_mut(index).map(|slot| {
                    slot.expiry = new_expiry;
                    std::mem::replace(&mut slot.value, value)
                });
                self.touch(index);
                return match old {
                    Some(old) => Inserted::Replaced(old),
                    None => Inserted::Added,
                };
            }
        }

        if ttl.is_some_and(|ttl| ttl.is_zero()) {
            return Inserted::Added;
        }

        let evicted = if self.entries.len() >= self.capacity {
            if now.is_some_and(|now| self.remove_one_expired(now)) {
                None
            } else {
                self.pop_lru_at(now)
            }
        } else {
            None
        };
        self.reserve_one();
        let expires_at = ttl.and_then(|ttl| now.and_then(|now| now.checked_add(ttl)));
        let (index, _) = self.entries.insert_full(
            key,
            Slot {
                value,
                newer: None,
                older: None,
                expiry: None,
            },
        );
        if let Some(expires_at) = expires_at {
            let expiry = self.allocate_expiry_key(expires_at);
            self.expiries.insert(expiry, index);
            if let Some(slot) = self.slot_mut(index) {
                slot.expiry = Some(expiry);
            }
        }
        self.push_front(index);
        match evicted {
            Some((key, value)) => Inserted::Evicted(key, value),
            None => Inserted::Added,
        }
    }

    /// Removes `key`, returning its value if it was present and not expired.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
        #[cfg(feature = "tokio")]
        if self.is_index_expired(index) {
            self.remove_index(index);
            return None;
        }
        self.remove_index(index).map(|(_, value)| value)
    }

    fn reserve_one(&mut self) {
        let len = self.entries.len();
        if len == self.entries.capacity() {
            let room = self.capacity.saturating_sub(len);
            self.entries.reserve_exact(len.max(4).min(room));
        }
    }
}

/// Iterates live entries from most to least recently used; reversed, from least
/// to most. Created by [`BoundedMap::iter`].
pub struct BoundedMapIter<'a, K, V> {
    entries: &'a IndexMap<K, Slot<V>>,
    front: Option<usize>,
    back: Option<usize>,
    remaining: usize,
    #[cfg(feature = "tokio")]
    now: Option<Instant>,
}

impl<K, V> BoundedMapIter<'_, K, V> {
    fn is_expired(&self, slot: &Slot<V>) -> bool {
        #[cfg(feature = "tokio")]
        {
            self.now.is_some_and(|now| slot.is_expired_at(now))
        }
        #[cfg(not(feature = "tokio"))]
        {
            let _ = slot;
            false
        }
    }
}

impl<'a, K, V> Iterator for BoundedMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            let (key, slot) = self.entries.get_index(self.front?)?;
            self.front = slot.older;
            if self.is_expired(slot) {
                continue;
            }
            self.remaining -= 1;
            return Some((key, &slot.value));
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> DoubleEndedIterator for BoundedMapIter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            let (key, slot) = self.entries.get_index(self.back?)?;
            self.back = slot.newer;
            if self.is_expired(slot) {
                continue;
            }
            self.remaining -= 1;
            return Some((key, &slot.value));
        }
        None
    }
}

impl<K, V> ExactSizeIterator for BoundedMapIter<'_, K, V> {}

impl<K, V> FusedIterator for BoundedMapIter<'_, K, V> {}

impl<K, V> Clone for BoundedMapIter<'_, K, V> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<'a, K, V> IntoIterator for &'a BoundedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = BoundedMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for BoundedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Entries<'a, K, V>(&'a BoundedMap<K, V>);
        impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Entries<'_, K, V> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_map().entries(self.0.iter()).finish()
            }
        }
        let mut debug = f.debug_struct("BoundedMap");
        debug.field("capacity", &self.capacity);
        #[cfg(feature = "tokio")]
        if let Some(ttl) = self.ttl {
            debug.field("ttl", &ttl);
        }
        debug.field("entries", &Entries(self)).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for BoundedMapIter<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}
