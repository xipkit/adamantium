use std::{
    borrow::Borrow,
    collections::HashMap,
    fmt,
    hash::{BuildHasher, Hash},
    sync::Arc,
};

use indexmap::IndexMap;

/// An insertion-ordered map with value semantics.
///
/// Cloning a `Map` shares its storage, and the first write to a shared map
/// copies it, so two names never observe each other's writes. Entries keep
/// the order they were inserted in. There is no `Index` implementation,
/// because indexing a missing key would panic; use [`get`](Self::get).
pub struct Map<K, V>(Arc<IndexMap<K, V>>);

impl<K, V> Map<K, V> {
    /// An empty map.
    pub fn new() -> Self {
        Self(Arc::new(IndexMap::new()))
    }

    /// An empty map with room for `capacity` entries before it grows.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Arc::new(IndexMap::with_capacity(capacity)))
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The entries in insertion order.
    pub fn iter(&self) -> indexmap::map::Iter<'_, K, V> {
        self.0.iter()
    }

    /// The keys in insertion order.
    pub fn keys(&self) -> indexmap::map::Keys<'_, K, V> {
        self.0.keys()
    }

    /// The values in insertion order.
    pub fn values(&self) -> indexmap::map::Values<'_, K, V> {
        self.0.values()
    }

    /// The entry at a position in insertion order.
    pub fn get_index(&self, index: usize) -> Option<(&K, &V)> {
        self.0.get_index(index)
    }

    /// The first entry in insertion order.
    pub fn first(&self) -> Option<(&K, &V)> {
        self.0.first()
    }

    /// The last entry in insertion order.
    pub fn last(&self) -> Option<(&K, &V)> {
        self.0.last()
    }

    /// The underlying `IndexMap`, for read operations not mirrored here.
    pub fn as_index_map(&self) -> &IndexMap<K, V> {
        &self.0
    }

    /// Whether another `Map` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl<K: Hash + Eq, V> Map<K, V> {
    /// The value for `key`, if present.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.0.get(key)
    }

    /// Whether `key` is present.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.0.contains_key(key)
    }
}

impl<K: Hash + Eq + Clone, V: Clone> Map<K, V> {
    /// Mutable access to the underlying `IndexMap`, copying it first if
    /// another `Map` shares it.
    pub fn make_mut(&mut self) -> &mut IndexMap<K, V> {
        Arc::make_mut(&mut self.0)
    }

    /// Inserts a value, returning the previous value for the key. An
    /// existing key keeps its position; a new key goes last.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.make_mut().insert(key, value)
    }

    /// Removes a key, keeping the order of the other entries.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.0.get_index_of(key)?;
        self.make_mut()
            .shift_remove_index(index)
            .map(|(_, value)| value)
    }

    /// Mutable access to the value for `key`, if present.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let index = self.0.get_index_of(key)?;
        self.make_mut().get_index_mut(index).map(|(_, value)| value)
    }

    /// Keeps only the entries for which `keep` returns `true`.
    pub fn retain(&mut self, keep: impl FnMut(&K, &mut V) -> bool) {
        self.make_mut().retain(keep);
    }

    /// Removes every entry, retaining capacity when uniquely owned.
    /// Shared storage is replaced with empty storage without cloning entries.
    pub fn clear(&mut self) {
        if let Some(entries) = Arc::get_mut(&mut self.0) {
            entries.clear();
        } else {
            *self = Self::new();
        }
    }

    /// The owned `IndexMap`. Free when nothing else shares the storage.
    pub fn into_index_map(self) -> IndexMap<K, V> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<K, V> Clone for Map<K, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<K, V> Default for Map<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> From<IndexMap<K, V>> for Map<K, V> {
    fn from(entries: IndexMap<K, V>) -> Self {
        Self(Arc::new(entries))
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> From<HashMap<K, V, S>> for Map<K, V> {
    fn from(entries: HashMap<K, V, S>) -> Self {
        Self(Arc::new(entries.into_iter().collect()))
    }
}

impl<K: Hash + Eq, V, const N: usize> From<[(K, V); N]> for Map<K, V> {
    fn from(entries: [(K, V); N]) -> Self {
        Self(Arc::new(IndexMap::from(entries)))
    }
}

impl<K: Hash + Eq + Clone, V: Clone> From<Map<K, V>> for IndexMap<K, V> {
    fn from(entries: Map<K, V>) -> Self {
        entries.into_index_map()
    }
}

impl<K: Hash + Eq, V> FromIterator<(K, V)> for Map<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl<K: Hash + Eq + Clone, V: Clone> Extend<(K, V)> for Map<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl<'a, K, V> IntoIterator for &'a Map<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = indexmap::map::Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<K: Hash + Eq + Clone, V: Clone> IntoIterator for Map<K, V> {
    type Item = (K, V);
    type IntoIter = indexmap::map::IntoIter<K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_index_map().into_iter()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Map<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// Equal when the same keys map to equal values, whatever the order.
impl<K: Hash + Eq, V: PartialEq> PartialEq for Map<K, V> {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl<K: Hash + Eq, V: Eq> Eq for Map<K, V> {}

#[cfg(feature = "serde")]
impl<K: serde::Serialize + Hash + Eq, V: serde::Serialize> serde::Serialize for Map<K, V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, K: serde::Deserialize<'de> + Hash + Eq, V: serde::Deserialize<'de>>
    serde::Deserialize<'de> for Map<K, V>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IndexMap::<K, V>::deserialize(deserializer).map(Self::from)
    }
}
