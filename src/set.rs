use std::{
    borrow::Borrow,
    collections::{HashSet, hash_map::RandomState},
    fmt,
    hash::{BuildHasher, Hash},
    sync::Arc,
};

use indexmap::IndexSet;

/// An insertion-ordered set of unique values with value semantics.
///
/// Cloning a `Set` shares its storage, and the first write to a shared set
/// copies it, so two names never observe each other's writes. Values keep
/// the order they were inserted in; inserting a duplicate keeps the original
/// value and its position. Equality compares membership, ignoring order,
/// just like [`Map`](crate::Map). Elements must not change their hash or
/// equality while stored in the set.
///
/// `Set<T>` is `Send + Sync` when `T` is. It is available without Tokio.
/// With the `serde` feature, sets serialize as sequences in insertion order;
/// deserialization keeps only the first occurrence of each value.
///
/// ```
/// use adamantium::Set;
///
/// let original = Set::from(["b", "a", "b"]);
/// let mut changed = original.clone();
/// changed.remove("b");
/// changed.insert("c");
/// assert_eq!(original.iter().copied().collect::<Vec<_>>(), ["b", "a"]);
/// assert_eq!(changed.iter().copied().collect::<Vec<_>>(), ["a", "c"]);
/// assert_eq!(original, Set::from(["a", "b"]));
/// ```
pub struct Set<T>(Arc<IndexSet<T>>);

impl<T> Set<T> {
    /// An empty set.
    pub fn new() -> Self {
        Self(Arc::new(IndexSet::new()))
    }

    /// An empty set with room for `capacity` values before it grows.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Arc::new(IndexSet::with_capacity(capacity)))
    }

    /// The number of values.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set has no values.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The values in insertion order.
    pub fn iter(&self) -> indexmap::set::Iter<'_, T> {
        self.0.iter()
    }

    /// The value at a position in insertion order, if any.
    pub fn get_index(&self, index: usize) -> Option<&T> {
        self.0.get_index(index)
    }

    /// The first value in insertion order.
    pub fn first(&self) -> Option<&T> {
        self.0.first()
    }

    /// The last value in insertion order.
    pub fn last(&self) -> Option<&T> {
        self.0.last()
    }

    /// The underlying `IndexSet`, for read operations not mirrored here.
    pub fn as_index_set(&self) -> &IndexSet<T> {
        &self.0
    }

    /// Whether another `Set` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl<T: Hash + Eq> Set<T> {
    /// The stored value equal to `value`, if present.
    pub fn get<Q>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.0.get(value)
    }

    /// Whether `value` is present.
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.0.contains(value)
    }

    /// Values in this set but not `other`, in this set's order.
    pub fn difference<'a>(
        &'a self,
        other: &'a Self,
    ) -> indexmap::set::Difference<'a, T, RandomState> {
        self.0.difference(&other.0)
    }

    /// Values in both sets, in this set's order.
    pub fn intersection<'a>(
        &'a self,
        other: &'a Self,
    ) -> indexmap::set::Intersection<'a, T, RandomState> {
        self.0.intersection(&other.0)
    }

    /// All values in this set's order, followed by new values in `other`'s order.
    pub fn union<'a>(&'a self, other: &'a Self) -> indexmap::set::Union<'a, T, RandomState> {
        self.0.union(&other.0)
    }

    /// Values in only one set: this set's values first, then `other`'s,
    /// preserving their respective orders.
    pub fn symmetric_difference<'a>(
        &'a self,
        other: &'a Self,
    ) -> indexmap::set::SymmetricDifference<'a, T, RandomState, RandomState> {
        self.0.symmetric_difference(&other.0)
    }

    /// Whether the sets have no values in common.
    pub fn is_disjoint(&self, other: &Self) -> bool {
        self.0.is_disjoint(&other.0)
    }

    /// Whether every value in this set is also in `other`.
    pub fn is_subset(&self, other: &Self) -> bool {
        self.0.is_subset(&other.0)
    }

    /// Whether every value in `other` is also in this set.
    pub fn is_superset(&self, other: &Self) -> bool {
        self.0.is_superset(&other.0)
    }
}

impl<T: Hash + Eq + Clone> Set<T> {
    /// Mutable access to the underlying `IndexSet`, copying it first if
    /// another `Set` shares it. Every write below goes through this.
    pub fn make_mut(&mut self) -> &mut IndexSet<T> {
        Arc::make_mut(&mut self.0)
    }

    /// Inserts a value, returning whether it was new. An existing value
    /// keeps its position; a new value goes last.
    pub fn insert(&mut self, value: T) -> bool {
        self.make_mut().insert(value)
    }

    /// Removes a value, returning whether it was present and keeping the
    /// order of the other values.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.make_mut().shift_remove(value)
    }

    /// Keeps only the values for which `keep` returns `true`, in their order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.make_mut().retain(keep);
    }

    /// Removes every value.
    pub fn clear(&mut self) {
        self.make_mut().clear();
    }

    /// The owned `IndexSet`. Free when nothing else shares the storage.
    pub fn into_index_set(self) -> IndexSet<T> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<T> Clone for Set<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for Set<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> From<IndexSet<T>> for Set<T> {
    fn from(values: IndexSet<T>) -> Self {
        Self(Arc::new(values))
    }
}

impl<T: Hash + Eq, S: BuildHasher> From<HashSet<T, S>> for Set<T> {
    /// Uses the source set's arbitrary iteration order as insertion order.
    fn from(values: HashSet<T, S>) -> Self {
        Self(Arc::new(values.into_iter().collect()))
    }
}

impl<T: Hash + Eq, const N: usize> From<[T; N]> for Set<T> {
    fn from(values: [T; N]) -> Self {
        Self(Arc::new(IndexSet::from(values)))
    }
}

impl<T: Hash + Eq + Clone> From<Set<T>> for IndexSet<T> {
    fn from(values: Set<T>) -> Self {
        values.into_index_set()
    }
}

impl<T: Hash + Eq> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl<T: Hash + Eq + Clone> Extend<T> for Set<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl<'a, T> IntoIterator for &'a Set<T> {
    type Item = &'a T;
    type IntoIter = indexmap::set::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Hash + Eq + Clone> IntoIterator for Set<T> {
    type Item = T;
    type IntoIter = indexmap::set::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_index_set().into_iter()
    }
}

impl<T: fmt::Debug> fmt::Debug for Set<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// Equal when the sets contain the same values, whatever the order.
impl<T: Hash + Eq> PartialEq for Set<T> {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl<T: Hash + Eq> Eq for Set<T> {}

#[cfg(feature = "serde")]
impl<T: serde::Serialize> serde::Serialize for Set<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T: serde::Deserialize<'de> + Hash + Eq> serde::Deserialize<'de> for Set<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IndexSet::<T>::deserialize(deserializer).map(Self::from)
    }
}
