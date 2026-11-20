use std::{
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::{Bound, Deref, Range, RangeBounds},
    sync::Arc,
};

use crate::Str;

/// A byte string with value semantics.
///
/// Cloning and slicing share storage. [`make_mut`](Self::make_mut) copies
/// only the visible bytes when the value is a window or shares its buffer.
/// Reads go through [`Deref`] to `[u8]`.
///
/// # Serde
///
/// With the `serde` feature, serializes as bytes. Deserialization accepts byte
/// sequences in human-readable formats and byte strings in binary formats.
/// JSON strings are rejected rather than interpreted as UTF-8 or base64.
///
/// Binary byte strings cannot currently be deserialized through Serde's value
/// buffering, including `#[serde(flatten)]`, `#[serde(untagged)]`, internally
/// tagged enums (`#[serde(tag = "...")]`), and adjacently tagged enums
/// (`#[serde(tag = "...", content = "...")]`) when the content comes before the
/// tag. Serde's buffer reports itself as human-readable, so `Bytes` requests a
/// sequence, which the buffer cannot provide for a byte string. Keeping this
/// sequence hint supports text formats such as quick-xml, csv, and envy, whose
/// `deserialize_any` does not produce a sequence.
#[derive(Clone, Default)]
pub struct Bytes {
    #[allow(
        clippy::rc_buffer,
        reason = "copy-on-write needs a growable buffer behind the Arc"
    )]
    storage: Arc<Vec<u8>>,
    window: Option<Range<usize>>,
}

impl Bytes {
    /// An empty byte string.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes as a slice.
    #[expect(
        clippy::indexing_slicing,
        reason = "slice validates window bounds and writes detach from windowed storage"
    )]
    pub fn as_slice(&self) -> &[u8] {
        match &self.window {
            Some(window) => &self.storage[window.clone()],
            None => &self.storage,
        }
    }

    /// Returns a byte window that shares this value's storage without copying.
    ///
    /// Bounds are relative to this value, even if it is itself a window.
    /// Bounds are converted to a half-open range. Empty ranges within bounds
    /// are valid. Reversed or out-of-bounds ranges return `None`. The window
    /// keeps the backing allocation alive.
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Option<Self> {
        let (offset, len) = match &self.window {
            Some(window) => (window.start, window.len()),
            None => (0, self.storage.len()),
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
            return Some(self.clone());
        }
        Some(Self {
            storage: Arc::clone(&self.storage),
            window: Some((offset + start)..(offset + end)),
        })
    }

    /// Mutable access to a `Vec<u8>` containing this value's bytes.
    /// A window or shared buffer is copied before mutation.
    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        if self.window.is_some() {
            *self = Self::from(self.as_slice());
        }
        Arc::make_mut(&mut self.storage)
    }

    /// Appends one byte.
    pub fn push(&mut self, byte: u8) {
        self.make_mut().push(byte);
    }

    /// Appends a slice.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.make_mut().extend_from_slice(bytes);
    }

    /// Removes every byte, retaining capacity when uniquely owned.
    /// Shared storage is replaced with empty storage without copying bytes.
    pub fn clear(&mut self) {
        if let Some(bytes) = Arc::get_mut(&mut self.storage) {
            bytes.clear();
            self.window = None;
        } else {
            *self = Self::new();
        }
    }

    /// Returns the contents as an owned `Vec<u8>`.
    /// Reuses the allocation when this value owns the entire buffer uniquely.
    pub fn into_vec(self) -> Vec<u8> {
        if self.window.is_some() {
            self.as_slice().to_vec()
        } else {
            Arc::unwrap_or_clone(self.storage)
        }
    }

    /// Whether another `Bytes` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.storage) > 1
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            storage: Arc::new(bytes),
            window: None,
        }
    }
}

impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Self {
        Self::from(bytes.to_vec())
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(bytes: [u8; N]) -> Self {
        Self::from(Vec::from(bytes))
    }
}

impl From<&str> for Bytes {
    fn from(s: &str) -> Self {
        Self::from(s.as_bytes())
    }
}

impl From<Str> for Bytes {
    fn from(s: Str) -> Self {
        Self::from(s.into_string().into_bytes())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(bytes: Bytes) -> Self {
        bytes.into_vec()
    }
}

impl FromIterator<u8> for Bytes {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl Extend<u8> for Bytes {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Bytes {}

impl PartialOrd for Bytes {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bytes {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl Hash for Bytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b\"{}\"", self.as_slice().escape_ascii())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.as_slice())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;

        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Bytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a byte string or a sequence of bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes::from(bytes))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes::from(bytes))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Bytes, A::Error> {
                <Vec<u8> as serde::Deserialize>::deserialize(
                    serde::de::value::SeqAccessDeserializer::new(seq),
                )
                .map(Bytes::from)
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_seq(BytesVisitor)
        } else {
            deserializer.deserialize_byte_buf(BytesVisitor)
        }
    }
}
