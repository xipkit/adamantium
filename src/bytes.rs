use std::{borrow::Borrow, fmt, ops::Deref, sync::Arc};

use crate::Str;

/// A byte string with value semantics.
///
/// Cloning shares storage, and the first write to shared storage copies it.
/// Reads go through [`Deref`] to `[u8]`.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(
    #[allow(
        clippy::rc_buffer,
        reason = "copy-on-write needs a growable buffer behind the Arc"
    )]
    Arc<Vec<u8>>,
);

impl Bytes {
    /// An empty byte string.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes as a slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Mutable access to the underlying `Vec<u8>`, copying it first if
    /// another `Bytes` shares it.
    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        Arc::make_mut(&mut self.0)
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
        if let Some(bytes) = Arc::get_mut(&mut self.0) {
            bytes.clear();
        } else {
            *self = Self::new();
        }
    }

    /// The owned `Vec<u8>`. Free when nothing else shares the storage.
    pub fn into_vec(self) -> Vec<u8> {
        Arc::unwrap_or_clone(self.0)
    }

    /// Whether another `Bytes` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }
}

impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Self {
        Self(Arc::new(bytes.to_vec()))
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(bytes: [u8; N]) -> Self {
        Self(Arc::new(bytes.to_vec()))
    }
}

impl From<&str> for Bytes {
    fn from(s: &str) -> Self {
        Self(Arc::new(s.as_bytes().to_vec()))
    }
}

impl From<Str> for Bytes {
    fn from(s: Str) -> Self {
        Self(Arc::new(s.into_string().into_bytes()))
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(bytes: Bytes) -> Self {
        bytes.into_vec()
    }
}

impl FromIterator<u8> for Bytes {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl Extend<u8> for Bytes {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b\"{}\"", self.0.escape_ascii())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<u8>::deserialize(deserializer).map(Self::from)
    }
}
