use std::{
    borrow::Borrow,
    cmp::Ordering,
    collections::{VecDeque, vec_deque},
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
};

/// A growable double-ended queue with value semantics.
///
/// Cloning a `Deque` shares its storage, and the first write to a shared
/// deque copies it, so two names never observe each other's writes. Reads
/// go through [`Deref`] to [`VecDeque`]; use [`get`](VecDeque::get) rather
/// than indexing, which can panic. Writes that take a position return `None`
/// or hand the value back instead of panicking.
///
/// ```
/// use adamantium::Deque;
///
/// let original = Deque::from([1, 2, 3]);
/// let mut window = original.clone();
/// assert_eq!(window.pop_front(), Some(1));
/// window.push_back(4);
/// assert_eq!(window.into_iter().collect::<Vec<_>>(), [2, 3, 4]);
/// assert_eq!(original.front(), Some(&1));
/// ```
///
/// A deque is `Send + Sync` when its elements are `Send + Sync`.
///
/// ```compile_fail
/// use adamantium::Deque;
/// use std::rc::Rc;
/// fn send<T: Send>() {}
/// send::<Deque<Rc<i32>>>();
/// ```
///
/// ```compile_fail
/// use adamantium::Deque;
/// use std::cell::Cell;
/// fn sync<T: Sync>() {}
/// sync::<Deque<Cell<i32>>>();
/// ```
pub struct Deque<T>(Arc<VecDeque<T>>);

impl<T> Deque<T> {
    /// An empty deque.
    pub fn new() -> Self {
        Self(Arc::new(VecDeque::new()))
    }

    /// An empty deque with room for `capacity` elements before it grows.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Arc::new(VecDeque::with_capacity(capacity)))
    }

    /// The underlying `VecDeque`, for read operations.
    pub fn as_vec_deque(&self) -> &VecDeque<T> {
        &self.0
    }

    /// Whether another `Deque` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl<T: Clone> Deque<T> {
    /// Mutable access to the underlying `VecDeque`, copying it first if
    /// another `Deque` shares it. Every write below goes through this.
    pub fn make_mut(&mut self) -> &mut VecDeque<T> {
        Arc::make_mut(&mut self.0)
    }

    /// Prepends an element.
    pub fn push_front(&mut self, value: T) {
        self.make_mut().push_front(value);
    }

    /// Appends an element.
    pub fn push_back(&mut self, value: T) {
        self.make_mut().push_back(value);
    }

    /// Removes and returns the first element, if any.
    pub fn pop_front(&mut self) -> Option<T> {
        self.make_mut().pop_front()
    }

    /// Removes and returns the last element, if any.
    pub fn pop_back(&mut self) -> Option<T> {
        self.make_mut().pop_back()
    }

    /// Mutable access to the first element, if any.
    pub fn front_mut(&mut self) -> Option<&mut T> {
        self.make_mut().front_mut()
    }

    /// Mutable access to the last element, if any.
    pub fn back_mut(&mut self) -> Option<&mut T> {
        self.make_mut().back_mut()
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
        self.make_mut().remove(index)
    }

    /// Replaces the element at `index`. Hands the value back if there is no
    /// such element.
    pub fn set(&mut self, index: usize, value: T) -> Result<(), T> {
        match self.get_mut(index) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(value),
        }
    }

    /// Mutable access to the element at `index`, if any.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.len() {
            return None;
        }
        self.make_mut().get_mut(index)
    }

    /// Keeps only the elements for which `keep` returns `true`.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.make_mut().retain(keep);
    }

    /// Shortens the deque to at most `len` elements.
    pub fn truncate(&mut self, len: usize) {
        self.make_mut().truncate(len);
    }

    /// Removes every element.
    pub fn clear(&mut self) {
        self.make_mut().clear();
    }

    /// The owned `VecDeque`. Free when nothing else shares the storage.
    pub fn into_vec_deque(self) -> VecDeque<T> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<T> Clone for Deque<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for Deque<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for Deque<T> {
    type Target = VecDeque<T>;

    fn deref(&self) -> &VecDeque<T> {
        &self.0
    }
}

impl<T> AsRef<VecDeque<T>> for Deque<T> {
    fn as_ref(&self) -> &VecDeque<T> {
        &self.0
    }
}

impl<T> Borrow<VecDeque<T>> for Deque<T> {
    fn borrow(&self) -> &VecDeque<T> {
        &self.0
    }
}

impl<T> From<VecDeque<T>> for Deque<T> {
    fn from(values: VecDeque<T>) -> Self {
        Self(Arc::new(values))
    }
}

impl<T> From<Vec<T>> for Deque<T> {
    fn from(values: Vec<T>) -> Self {
        Self::from(VecDeque::from(values))
    }
}

impl<T: Clone> From<&[T]> for Deque<T> {
    fn from(values: &[T]) -> Self {
        Self::from(values.to_vec())
    }
}

impl<T, const N: usize> From<[T; N]> for Deque<T> {
    fn from(values: [T; N]) -> Self {
        Self::from(VecDeque::from(values))
    }
}

impl<T: Clone> From<Deque<T>> for VecDeque<T> {
    fn from(values: Deque<T>) -> Self {
        values.into_vec_deque()
    }
}

impl<T: Clone> From<Deque<T>> for Vec<T> {
    fn from(values: Deque<T>) -> Self {
        values.into_vec_deque().into()
    }
}

impl<T> FromIterator<T> for Deque<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<VecDeque<T>>())
    }
}

impl<T: Clone> Extend<T> for Deque<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl<'a, T> IntoIterator for &'a Deque<T> {
    type Item = &'a T;
    type IntoIter = vec_deque::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Clone> IntoIterator for Deque<T> {
    type Item = T;
    type IntoIter = vec_deque::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec_deque().into_iter()
    }
}

impl<T: fmt::Debug> fmt::Debug for Deque<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl<T: PartialEq> PartialEq for Deque<T> {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl<T: Eq> Eq for Deque<T> {}

impl<T: PartialOrd> PartialOrd for Deque<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl<T: Ord> Ord for Deque<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T: Hash> Hash for Deque<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[cfg(feature = "serde")]
impl<T: serde::Serialize> serde::Serialize for Deque<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Deque<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        VecDeque::<T>::deserialize(deserializer).map(Self::from)
    }
}
