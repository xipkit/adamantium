use std::{borrow::Borrow, fmt, hash::Hash, iter::FusedIterator};

use indexmap::IndexMap;

/// A map that evicts the least recently used entry to stay within capacity.
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
/// least recently used entry in [`Inserted::Evicted`]. At zero capacity,
/// every insert returns the entry just offered.
///
/// # Costs
///
/// Lookups, inserts, and removals take amortized O(1) time. Storage grows on
/// demand up to capacity and is kept for reuse. Cloning copies every entry.
#[derive(Clone)]
pub struct BoundedMap<K, V> {
    entries: IndexMap<K, Slot<V>>,
    capacity: usize,
    head: Option<usize>,
    tail: Option<usize>,
}

#[derive(Clone)]
struct Slot<V> {
    value: V,
    newer: Option<usize>,
    older: Option<usize>,
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
        }
    }

    /// The most entries the map holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the map holds `capacity` entries, so that inserting a new key
    /// would evict one.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// The entries from most to least recently used, without changing the
    /// order. Reverse it to start from the next entry to be evicted.
    pub fn iter(&self) -> BoundedMapIter<'_, K, V> {
        BoundedMapIter {
            entries: &self.entries,
            front: self.head,
            back: self.tail,
            remaining: self.entries.len(),
        }
    }

    /// Removes every entry, keeping the capacity and the storage.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.head = None;
        self.tail = None;
    }

    /// Removes and returns the least recently used entry, the one the next
    /// insert of a new key into a full map would evict.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        let tail = self.tail?;
        self.remove_index(tail)
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
        if let Some(&Slot { newer, older, .. }) = self.slot(index) {
            match newer.and_then(|newer| self.slot_mut(newer)) {
                Some(slot) => slot.older = Some(index),
                None => self.head = Some(index),
            }
            match older.and_then(|older| self.slot_mut(older)) {
                Some(slot) => slot.newer = Some(index),
                None => self.tail = Some(index),
            }
        }
        Some((key, slot.value))
    }
}

impl<K: Hash + Eq, V> BoundedMap<K, V> {
    /// The value for `key`, which becomes the most recently used entry.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
        self.touch(index);
        self.slot(index).map(|slot| &slot.value)
    }

    /// Mutable access to the value for `key`, which becomes the most
    /// recently used entry.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
        self.touch(index);
        self.slot_mut(index).map(|slot| &mut slot.value)
    }

    /// The value for `key`, without changing the order of use.
    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.entries.get(key).map(|slot| &slot.value)
    }

    /// Whether `key` is present, without changing the order of use.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.entries.contains_key(key)
    }

    /// Inserts `value` for `key` and makes it the most recently used entry.
    ///
    /// A present key keeps its stored key and gets the new value. A new key
    /// in a full map first evicts the least recently used entry. The result
    /// says which of these happened and returns whatever left the map.
    pub fn insert(&mut self, key: K, value: V) -> Inserted<K, V> {
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

    /// Removes `key`, returning its value.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.entries.get_index_of(key)?;
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

/// Iterates from most to least recently used; reversed, from least to most.
/// Created by [`BoundedMap::iter`].
pub struct BoundedMapIter<'a, K, V> {
    entries: &'a IndexMap<K, Slot<V>>,
    front: Option<usize>,
    back: Option<usize>,
    remaining: usize,
}

impl<'a, K, V> Iterator for BoundedMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (key, slot) = self.entries.get_index(self.front?)?;
        self.remaining -= 1;
        self.front = slot.older;
        Some((key, &slot.value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> DoubleEndedIterator for BoundedMapIter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (key, slot) = self.entries.get_index(self.back?)?;
        self.remaining -= 1;
        self.back = slot.newer;
        Some((key, &slot.value))
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
        f.debug_struct("BoundedMap")
            .field("capacity", &self.capacity)
            .field("entries", &Entries(self))
            .finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for BoundedMapIter<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}
