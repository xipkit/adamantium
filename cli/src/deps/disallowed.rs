use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use toml_edit::{DocumentMut, Value};

use crate::lints::CLIPPY_TOML;

/// One entry of `disallowed-methods`.
#[derive(Debug)]
pub struct Entry {
    pub path: String,
    pub reason: String,
}

/// The `disallowed-methods` entries, indexed for matching.
#[derive(Debug)]
pub struct Disallowed {
    pub entries: Vec<Entry>,
    by_path: HashMap<Vec<String>, usize>,
    crates: HashSet<String>,
    names: HashSet<String>,
    method_names: HashSet<String>,
}

impl Disallowed {
    /// Reads the entries from Adamantium's bundled `clippy.toml`.
    pub fn bundled() -> Result<Self> {
        Self::parse(CLIPPY_TOML)
    }

    fn parse(text: &str) -> Result<Self> {
        let doc: DocumentMut = text.parse().context("parsing clippy.toml")?;
        let list = doc
            .get("disallowed-methods")
            .and_then(|item| item.as_array())
            .ok_or_else(|| anyhow!("clippy.toml has no disallowed-methods array"))?;
        let mut disallowed = Self {
            entries: Vec::new(),
            by_path: HashMap::new(),
            crates: HashSet::new(),
            names: HashSet::new(),
            method_names: HashSet::new(),
        };
        for value in list {
            let (path, reason) = match value {
                Value::String(path) => (path.value().clone(), String::new()),
                Value::InlineTable(table) => (
                    table
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("a disallowed-methods entry has no path"))?
                        .to_owned(),
                    table
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
                _ => return Err(anyhow!("unexpected disallowed-methods entry: {value}")),
            };
            let segments = canonical(path.split("::").map(str::to_owned).collect());
            let (Some(name), Some(krate)) = (segments.last(), segments.first()) else {
                return Err(anyhow!("an empty path in disallowed-methods"));
            };
            disallowed.crates.insert(krate.clone());
            disallowed.names.insert(name.clone());
            if names_a_method(&segments) {
                disallowed.method_names.insert(name.clone());
            }
            disallowed
                .by_path
                .insert(segments, disallowed.entries.len());
            disallowed.entries.push(Entry { path, reason });
        }
        Ok(disallowed)
    }

    /// The index of the entry naming this canonical path.
    pub fn entry(&self, path: &[String]) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// Every entry's canonical path, with its index.
    pub fn paths(&self) -> impl Iterator<Item = (&[String], usize)> {
        self.by_path
            .iter()
            .map(|(path, index)| (path.as_slice(), *index))
    }

    /// Whether `krate` is a crate that some entry names. Paths into these
    /// crates are compared exactly; a path into another crate may reach a
    /// listed item through a re-export that the audit can't see.
    pub fn names_crate(&self, krate: &str) -> bool {
        self.crates.contains(krate)
    }

    /// Whether some entry's item has this name.
    pub fn has_name(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Whether some entry names a method with this name, which a method call
    /// on a value of the right type would reach.
    pub fn has_method(&self, name: &str) -> bool {
        self.method_names.contains(name)
    }
}

/// A path with the standard library's facades unified: `core` and `alloc`
/// items are the ones `std` re-exports.
pub fn canonical(mut path: Vec<String>) -> Vec<String> {
    if let Some(first) = path.first_mut()
        && (first == "core" || first == "alloc")
    {
        "std".clone_into(first);
    }
    path
}

fn names_a_method(path: &[String]) -> bool {
    let parent = path.len().checked_sub(2).and_then(|index| path.get(index));
    parent.is_some_and(|parent| {
        parent == "slice" || parent.chars().next().is_some_and(char::is_uppercase)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> Vec<String> {
        text.split("::").map(str::to_owned).collect()
    }

    #[test]
    fn reads_every_entry_of_the_bundled_clippy_toml() {
        let disallowed = Disallowed::bundled().unwrap();
        let doc: DocumentMut = CLIPPY_TOML.parse().unwrap();
        let count = doc["disallowed-methods"].as_array().unwrap().len();
        assert_eq!(disallowed.entries.len(), count);
        let spawn = disallowed.entry(&path("tokio::task::spawn")).unwrap();
        assert_eq!(disallowed.entries[spawn].path, "tokio::task::spawn");
        assert!(
            disallowed.entries[spawn]
                .reason
                .contains("adamantium::Tasks")
        );
    }

    #[test]
    fn std_facades_are_unified() {
        let disallowed = Disallowed::bundled().unwrap();
        assert_eq!(
            disallowed.entry(&canonical(path("core::mem::forget"))),
            disallowed.entry(&path("std::mem::forget"))
        );
        assert!(disallowed.entry(&path("std::mem::forget")).is_some());
    }

    #[test]
    fn only_associated_functions_are_methods() {
        let disallowed = Disallowed::bundled().unwrap();
        assert!(disallowed.has_method("spawn"));
        assert!(disallowed.has_method("split_at"));
        assert!(disallowed.has_method("insert"));
        assert!(!disallowed.has_method("forget"));
        assert!(!disallowed.has_method("sleep_until"));
        assert!(disallowed.has_name("forget"));
    }
}
