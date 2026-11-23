use std::{
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::{Bound, Deref, Range, RangeBounds},
    sync::Arc,
};

/// A string with value semantics.
///
/// Cloning and slicing share storage. [`make_mut`](Self::make_mut) copies
/// only the visible text when the value is a window or shares its buffer.
/// Two names never observe each other's writes. Reads go through [`Deref`]
/// to `str`.
#[derive(Clone, Default)]
pub struct Str {
    #[allow(
        clippy::rc_buffer,
        reason = "copy-on-write needs a growable buffer behind the Arc"
    )]
    storage: Arc<String>,
    window: Option<Range<usize>>,
}

impl Str {
    /// An empty string.
    pub fn new() -> Self {
        Self::default()
    }

    /// The string as a `str`.
    #[expect(
        clippy::string_slice,
        reason = "slice validates window bounds and UTF-8 char boundaries, and writes detach from windowed storage"
    )]
    pub fn as_str(&self) -> &str {
        match &self.window {
            Some(window) => &self.storage[window.clone()],
            None => &self.storage,
        }
    }

    /// Returns a string window that shares this value's storage without copying.
    ///
    /// Bounds are byte offsets relative to this value, even if it is itself a
    /// window. Bounds are converted to a half-open range. Empty ranges on UTF-8
    /// char boundaries within bounds are valid. Reversed, out-of-bounds, or
    /// non-char-boundary ranges return `None`. The window keeps the backing
    /// allocation alive.
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Option<Self> {
        match select(self.window.as_ref(), self.storage.len(), range)? {
            Selection::Whole => Some(self.clone()),
            Selection::Part(part) => {
                if !self.storage.is_char_boundary(part.start)
                    || !self.storage.is_char_boundary(part.end)
                {
                    return None;
                }
                Some(Self {
                    storage: Arc::clone(&self.storage),
                    window: Some(part),
                })
            }
        }
    }

    /// Mutable access to a `String` containing this value's text.
    /// A window or shared buffer is copied before mutation.
    pub fn make_mut(&mut self) -> &mut String {
        if self.window.is_some() {
            *self = Self::from(self.as_str());
        }
        Arc::make_mut(&mut self.storage)
    }

    /// Appends a string slice.
    pub fn push_str(&mut self, s: &str) {
        self.make_mut().push_str(s);
    }

    /// Appends a character.
    pub fn push(&mut self, c: char) {
        self.make_mut().push(c);
    }

    /// Removes every character, retaining capacity when uniquely owned.
    /// Shared storage is replaced with empty storage without copying characters.
    pub fn clear(&mut self) {
        if let Some(string) = Arc::get_mut(&mut self.storage) {
            string.clear();
            self.window = None;
        } else {
            *self = Self::new();
        }
    }

    /// Returns the contents as an owned `String`.
    /// Reuses the allocation when this value owns the entire buffer uniquely.
    pub fn into_string(self) -> String {
        if self.window.is_some() {
            self.as_str().to_owned()
        } else {
            Arc::unwrap_or_clone(self.storage)
        }
    }

    /// Whether another `Str` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.storage) > 1
    }
}

impl Deref for Str {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Str {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for Str {
    fn as_ref(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl Borrow<str> for Str {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Str {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}

impl From<String> for Str {
    fn from(s: String) -> Self {
        Self {
            storage: Arc::new(s),
            window: None,
        }
    }
}

impl From<char> for Str {
    fn from(c: char) -> Self {
        Self::from(c.to_string())
    }
}

impl From<Str> for String {
    fn from(s: Str) -> Self {
        s.into_string()
    }
}

impl PartialEq for Str {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Str {}

impl PartialOrd for Str {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Str {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for Str {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for Str {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Str {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Str {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl std::ops::AddAssign<&str> for Str {
    fn add_assign(&mut self, rhs: &str) {
        self.push_str(rhs);
    }
}

impl FromIterator<char> for Str {
    fn from_iter<I: IntoIterator<Item = char>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<String>())
    }
}

impl<'a> FromIterator<&'a str> for Str {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<String>())
    }
}

impl Extend<char> for Str {
    fn extend<I: IntoIterator<Item = char>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl<'a> Extend<&'a str> for Str {
    fn extend<I: IntoIterator<Item = &'a str>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl fmt::Display for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl fmt::Debug for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Str {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Str {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

// What a `slice` call selects from a value.
pub(crate) enum Selection {
    Whole,
    // A range in the backing storage.
    Part(Range<usize>),
}

// Resolves `range`, which is relative to a value that shows `window` of a
// storage of `storage_len` bytes. A reversed or out-of-bounds range is `None`.
pub(crate) fn select(
    window: Option<&Range<usize>>,
    storage_len: usize,
    range: impl RangeBounds<usize>,
) -> Option<Selection> {
    let (offset, len) = match window {
        Some(window) => (window.start, window.len()),
        None => (0, storage_len),
    };
    let start = match range.start_bound() {
        Bound::Included(&start) => start,
        Bound::Excluded(&start) => start.checked_add(1)?,
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&end) => end.checked_add(1)?,
        Bound::Excluded(&end) => end,
        Bound::Unbounded => len,
    };
    if start > end || end > len {
        return None;
    }
    if start == 0 && end == len {
        return Some(Selection::Whole);
    }
    Some(Selection::Part((offset + start)..(offset + end)))
}
