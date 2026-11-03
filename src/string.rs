use std::{borrow::Borrow, fmt, ops::Deref, sync::Arc};

/// A string with value semantics.
///
/// Cloning a `Str` shares its storage, and the first write to a shared
/// string copies it. Two names never observe each other's writes. Reads go
/// through [`Deref`] to `str`.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Str(
    #[allow(
        clippy::rc_buffer,
        reason = "copy-on-write needs a growable buffer behind the Arc"
    )]
    Arc<String>,
);

impl Str {
    /// An empty string.
    pub fn new() -> Self {
        Self::default()
    }

    /// The string as a `str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mutable access to the underlying `String`, copying it first if another
    /// `Str` shares it.
    pub fn make_mut(&mut self) -> &mut String {
        Arc::make_mut(&mut self.0)
    }

    /// Appends a string slice.
    pub fn push_str(&mut self, s: &str) {
        self.make_mut().push_str(s);
    }

    /// Appends a character.
    pub fn push(&mut self, c: char) {
        self.make_mut().push(c);
    }

    /// Removes every character.
    pub fn clear(&mut self) {
        self.make_mut().clear();
    }

    /// The owned `String`. Free when nothing else shares the storage.
    pub fn into_string(self) -> String {
        Arc::unwrap_or_clone(self.0)
    }

    /// Whether another `Str` currently shares this one's storage.
    pub fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

impl Deref for Str {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Str {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<[u8]> for Str {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl Borrow<str> for Str {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Str {
    fn from(s: &str) -> Self {
        Self(Arc::new(s.to_owned()))
    }
}

impl From<String> for Str {
    fn from(s: String) -> Self {
        Self(Arc::new(s))
    }
}

impl From<char> for Str {
    fn from(c: char) -> Self {
        Self(Arc::new(c.to_string()))
    }
}

impl From<Str> for String {
    fn from(s: Str) -> Self {
        s.into_string()
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
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl<'a> FromIterator<&'a str> for Str {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
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
