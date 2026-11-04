//! The skip list a project keeps in its `Cargo.toml`, so that it can adopt
//! the rules one at a time and exempt the places where a rule does not
//! apply:
//!
//! ```toml
//! [workspace.metadata.adamantium]
//! skip = ["tests/", "examples/"]              # nothing is checked here
//! allow = ["indexing_slicing", "unwrap_used"] # these rules are off, for now
//!
//! [workspace.metadata.adamantium.exceptions]
//! future_not_send = ["src/syntax/", "src/bytecode.rs"] # this rule is off here
//! ```
//!
//! A single package uses `[package.metadata.adamantium]`. Paths are globs
//! relative to the directory holding `Cargo.toml`, and a path without
//! wildcards also names everything under it. Rules go by their bare name or
//! by the qualified one `cargo adamantium rules` prints. The list applies to
//! `cargo adamantium check`; a `[lints]` table cannot scope a rule to a path.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use toml_edit::{DocumentMut, Item};

use crate::lints::{LINTS, OVERFLOW_RULE};

/// A project's skip list. The default skips nothing.
#[derive(Debug)]
pub struct Config {
    /// The table the list came from, such as `workspace.metadata.adamantium`.
    pub table: Option<String>,
    skip: GlobSet,
    allow: HashSet<String>,
    exceptions: BTreeMap<String, GlobSet>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            table: None,
            skip: GlobSet::empty(),
            allow: HashSet::new(),
            exceptions: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Reads the skip list from the `Cargo.toml` in `root`.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join("Cargo.toml");
        let doc: DocumentMut = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;
        Config::parse(&doc)
    }

    fn parse(doc: &DocumentMut) -> Result<Config> {
        let found = ["workspace", "package"].into_iter().find_map(|top| {
            let item = doc.get(top)?.get("metadata")?.get("adamantium")?;
            Some((format!("{top}.metadata.adamantium"), item))
        });
        let Some((table_name, item)) = found else {
            return Ok(Config::default());
        };
        let table = item
            .as_table_like()
            .ok_or_else(|| anyhow!("[{table_name}] in Cargo.toml is not a table"))?;
        for (key, _) in table.iter() {
            if !matches!(key, "skip" | "allow" | "exceptions") {
                bail!(
                    "unknown key `{key}` in [{table_name}]; the keys are skip, allow and exceptions"
                );
            }
        }
        let skip = globs(table.get("skip"), &format!("{table_name}.skip"))?;
        let mut allow = HashSet::new();
        for name in strings(table.get("allow"), &format!("{table_name}.allow"))? {
            allow.insert(qualify(&name, &table_name)?);
        }
        let mut exceptions = BTreeMap::new();
        if let Some(item) = table.get("exceptions") {
            let section = format!("{table_name}.exceptions");
            let entries = item
                .as_table_like()
                .ok_or_else(|| anyhow!("[{section}] in Cargo.toml is not a table"))?;
            for (rule, paths) in entries.iter() {
                let set = globs(Some(paths), &format!("{section}.{rule}"))?;
                exceptions.insert(qualify(rule, &section)?, set);
            }
        }
        Ok(Config {
            table: Some(table_name),
            skip,
            allow,
            exceptions,
        })
    }

    /// Whether a finding of `rule` at `file`, a path relative to the project
    /// root, is left out of the report.
    pub fn skips(&self, rule: &str, file: &str) -> bool {
        self.skip.is_match(file)
            || self.allow.contains(rule)
            || self
                .exceptions
                .get(rule)
                .is_some_and(|paths| paths.is_match(file))
    }

    /// Whether the project applies `rule` everywhere, so that a `[lints]`
    /// table can carry it.
    pub fn adopted(&self, rule: &str) -> bool {
        !self.allow.contains(rule) && !self.exceptions.contains_key(rule)
    }

    /// Whether some paths are skipped outright.
    pub fn skips_paths(&self) -> bool {
        !self.skip.is_empty()
    }
}

/// The strings in an array item, or none when the item is absent.
fn strings(item: Option<&Item>, key: &str) -> Result<Vec<String>> {
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let wrong = || anyhow!("`{key}` in Cargo.toml is not an array of strings");
    let array = item.as_array().ok_or_else(wrong)?;
    array
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or_else(wrong))
        .collect()
}

/// Globs for the paths in an array item. A path without wildcards also names
/// everything under it, so `tests/`, `tests` and `tests/**` all cover the
/// directory.
fn globs(item: Option<&Item>, key: &str) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for path in strings(item, key)? {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            bail!("`{key}` in Cargo.toml has an empty path");
        }
        set.add(glob(path, key)?);
        if !path.contains(&['*', '?', '[', '{'][..]) {
            set.add(glob(&format!("{path}/**"), key)?);
        }
    }
    set.build()
        .with_context(|| format!("`{key}` in Cargo.toml"))
}

fn glob(pattern: &str, key: &str) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .with_context(|| format!("`{key}` in Cargo.toml has an invalid glob {pattern:?}"))
}

/// The qualified name of a rule given by either of its names.
fn qualify(name: &str, table: &str) -> Result<String> {
    if name == OVERFLOW_RULE || Some(name) == OVERFLOW_RULE.strip_prefix("adamantium::") {
        return Ok(OVERFLOW_RULE.to_owned());
    }
    LINTS
        .iter()
        .find(|lint| lint.name == name || lint.qualified() == name)
        .map(|lint| lint.qualified())
        .ok_or_else(|| {
            anyhow!("unknown rule `{name}` in [{table}]; `cargo adamantium rules` lists them")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Config> {
        Config::parse(&toml.parse().unwrap())
    }

    #[test]
    fn a_manifest_without_the_table_skips_nothing() {
        let config = parse("[package]\nname = \"demo\"\n").unwrap();
        assert!(config.table.is_none());
        assert!(!config.skips("clippy::unwrap_used", "src/lib.rs"));
        assert!(config.adopted("clippy::unwrap_used"));
        assert!(!config.skips_paths());
    }

    #[test]
    fn paths_rules_and_exceptions_each_skip_their_own_findings() {
        let config = parse(
            "[workspace]\n\
             [workspace.metadata.adamantium]\n\
             skip = [\"tests/\", \"examples\", \"benches/**/*.rs\"]\n\
             allow = [\"indexing_slicing\", \"clippy::unwrap_used\", \"unsafe_code\", \"overflow_checks\"]\n\
             [workspace.metadata.adamantium.exceptions]\n\
             future_not_send = [\"src/syntax/\", \"src/bytecode.rs\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.table.as_deref(),
            Some("workspace.metadata.adamantium")
        );
        assert!(config.skips("clippy::panic", "tests/a/b.rs"));
        assert!(config.skips("clippy::panic", "examples/demo.rs"));
        assert!(config.skips("clippy::panic", "benches/deep/run.rs"));
        assert!(!config.skips("clippy::panic", "src/tests.rs"));
        assert!(config.skips("clippy::indexing_slicing", "src/lib.rs"));
        assert!(config.skips("clippy::unwrap_used", "src/lib.rs"));
        assert!(config.skips("unsafe_code", "src/lib.rs"));
        assert!(config.skips(OVERFLOW_RULE, "Cargo.toml"));
        assert!(config.skips("clippy::future_not_send", "src/syntax/work.rs"));
        assert!(config.skips("clippy::future_not_send", "src/bytecode.rs"));
        assert!(!config.skips("clippy::future_not_send", "src/vm.rs"));
        assert!(!config.adopted("clippy::future_not_send"));
        assert!(!config.adopted("clippy::indexing_slicing"));
        assert!(config.adopted("clippy::panic"));
        assert!(config.skips_paths());
    }

    #[test]
    fn the_package_table_is_read_when_the_workspace_has_none() {
        let config = parse(
            "[workspace]\n[package]\nname = \"demo\"\n[package.metadata.adamantium]\nallow = [\"panic\"]\n",
        )
        .unwrap();
        assert_eq!(config.table.as_deref(), Some("package.metadata.adamantium"));
        assert!(config.skips("clippy::panic", "src/lib.rs"));
    }

    #[test]
    fn unknown_rules_and_keys_are_errors() {
        let error = parse("[package]\n[package.metadata.adamantium]\nallow = [\"no_such_rule\"]\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown rule `no_such_rule`"), "{error}");
        let error = parse("[package]\n[package.metadata.adamantium]\nignore = [\"tests/\"]\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown key `ignore`"), "{error}");
        let error = parse("[package]\n[package.metadata.adamantium]\nskip = \"tests/\"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an array of strings"), "{error}");
    }
}
