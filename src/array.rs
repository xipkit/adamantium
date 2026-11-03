use std::{
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
};

/// A growable list with value semantics.
///
/// Cloning an `Array` shares its storage, and the first write to a shared
/// array copies it, so two names never observe each other's writes. Reads
/// go through [`Deref`] to `[T]`; use [`get`](slice::get) rather than
/// indexing, which can panic. Writes that take a position return `None` or
/// hand the value back instead of panicking.
pub struct Array<T>(
    #[allow(
        clippy::rc_buffer,
        reason = "copy-on-write needs a growable buffer behind the Arc"
    )]
    Arc<Vec<T>>,
);

impl<T> Array<T> {
    /// An empty array.
    pub fn new() -> Self {
        Self(Arc::new(Vec::new()))
    }

    /// An empty array with room for `capacity` elements before it grows.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Arc::new(Vec::with_capacity(capacity)))
    }

    /// The elements as a slice.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Whether another `Array` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl<T: Clone> Array<T> {
    /// Mutable access to the underlying `Vec`, copying it first if another
    /// `Array` shares it. Every write below goes through this.
    pub fn make_mut(&mut self) -> &mut Vec<T> {
        Arc::make_mut(&mut self.0)
    }

    /// Appends an element.
    pub fn push(&mut self, value: T) {
        self.make_mut().push(value);
    }

    /// Removes and returns the last element, if any.
    pub fn pop(&mut self) -> Option<T> {
        self.make_mut().pop()
    }

    /// Inserts `value` at `index`, shifting later elements. Hands the value
    /// back if `index` is past the end.
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), T> {
        if index > self.len() {
            return Err(value);
        }
        self.make_mut().insert(index, value);
        Ok(())
    }

    /// Removes and returns the element at `index`, shifting later elements.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len() {
            return None;
        }
        Some(self.make_mut().remove(index))
    }

    /// Replaces the element at `index`. Hands the value back if there is no
    /// such element.
    pub fn set(&mut self, index: usize, value: T) -> Result<(), T> {
        match self.make_mut().get_mut(index) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(value),
        }
    }

    /// Mutable access to the element at `index`, if any.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.make_mut().get_mut(index)
    }

    /// Keeps only the elements for which `keep` returns `true`.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.make_mut().retain(keep);
    }

    /// Shortens the array to at most `len` elements.
    pub fn truncate(&mut self, len: usize) {
        self.make_mut().truncate(len);
    }

    /// Removes every element.
    pub fn clear(&mut self) {
        self.make_mut().clear();
    }

    /// Reverses the order of the elements in place.
    pub fn reverse(&mut self) {
        self.make_mut().reverse();
    }

    /// Sorts the elements in place.
    pub fn sort(&mut self)
    where
        T: Ord,
    {
        self.make_mut().sort();
    }

    /// Sorts the elements in place by a comparison function.
    pub fn sort_by(&mut self, compare: impl FnMut(&T, &T) -> Ordering) {
        self.make_mut().sort_by(compare);
    }

    /// Sorts the elements in place by a key.
    pub fn sort_by_key<K: Ord>(&mut self, key: impl FnMut(&T) -> K) {
        self.make_mut().sort_by_key(key);
    }

    /// The owned `Vec`. Free when nothing else shares the storage.
    pub fn into_vec(self) -> Vec<T> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<T> Clone for Array<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for Array<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for Array<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> AsRef<[T]> for Array<T> {
    fn as_ref(&self) -> &[T] {
        &self.0
    }
}

impl<T> Borrow<[T]> for Array<T> {
    fn borrow(&self) -> &[T] {
        &self.0
    }
}

impl<T> From<Vec<T>> for Array<T> {
    fn from(values: Vec<T>) -> Self {
        Self(Arc::new(values))
    }
}

impl<T: Clone> From<&[T]> for Array<T> {
    fn from(values: &[T]) -> Self {
        Self(Arc::new(values.to_vec()))
    }
}

impl<T, const N: usize> From<[T; N]> for Array<T> {
    fn from(values: [T; N]) -> Self {
        Self(Arc::new(values.into()))
    }
}

impl<T: Clone> From<Array<T>> for Vec<T> {
    fn from(values: Array<T>) -> Self {
        values.into_vec()
    }
}

impl<T> FromIterator<T> for Array<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl<T: Clone> Extend<T> for Array<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl<'a, T> IntoIterator for &'a Array<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Clone> IntoIterator for Array<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<T: fmt::Debug> fmt::Debug for Array<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: PartialEq> PartialEq for Array<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for Array<T> {}

impl<T: PartialOrd> PartialOrd for Array<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_slice().partial_cmp(other.as_slice())
    }
}

impl<T: Ord> Ord for Array<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl<T: Hash> Hash for Array<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

#[cfg(feature = "serde")]
impl<T: serde::Serialize> serde::Serialize for Array<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Array<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<T>::deserialize(deserializer).map(Self::from)
    }
}
