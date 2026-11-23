use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use toml_edit::{DocumentMut, Item};

use crate::{
    lints::{self, EXPERIMENTAL_LINTS, OVERFLOW_RULE},
    manifest,
};

/// A project's command defaults and skip list.
#[derive(Debug)]
pub struct Config {
    pub table: Option<String>,
    pub strict: bool,
    pub baseline: Option<PathBuf>,
    pub deps_baseline: Option<PathBuf>,
    skip: GlobSet,
    allow: HashSet<String>,
    exceptions: BTreeMap<String, GlobSet>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            table: None,
            strict: false,
            baseline: None,
            deps_baseline: None,
            skip: GlobSet::empty(),
            allow: HashSet::new(),
            exceptions: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Reads project settings from the `Cargo.toml` in `root`.
    pub fn load(root: &Path) -> Result<Config> {
        let (_, doc) = manifest::load(root)?;
        Config::parse(&doc, root)
    }

    fn parse(doc: &DocumentMut, root: &Path) -> Result<Config> {
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
            if !matches!(
                key,
                "skip" | "allow" | "exceptions" | "strict" | "baseline" | "deps"
            ) {
                bail!(
                    "unknown key `{key}` in [{table_name}]; the keys are skip, allow, exceptions, strict, baseline and deps"
                );
            }
        }
        let strict = if let Some(item) = table.get("strict") {
            item.as_bool()
                .ok_or_else(|| anyhow!("`{table_name}.strict` in Cargo.toml is not a boolean"))?
        } else {
            false
        };
        let baseline = baseline_path(
            table.get("baseline"),
            root,
            &format!("{table_name}.baseline"),
        )?;
        let deps_baseline = if let Some(item) = table.get("deps") {
            let section = format!("{table_name}.deps");
            let deps = item
                .as_table_like()
                .ok_or_else(|| anyhow!("[{section}] in Cargo.toml is not a table"))?;
            for (key, _) in deps.iter() {
                if key != "baseline" {
                    bail!("unknown key `{key}` in [{section}]; the only key is baseline");
                }
            }
            baseline_path(deps.get("baseline"), root, &format!("{section}.baseline"))?
        } else {
            None
        };
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
            strict,
            baseline,
            deps_baseline,
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

fn baseline_path(item: Option<&Item>, root: &Path, key: &str) -> Result<Option<PathBuf>> {
    let Some(item) = item else {
        return Ok(None);
    };
    let path = item
        .as_str()
        .ok_or_else(|| anyhow!("`{key}` in Cargo.toml is not a path string"))?;
    if path.is_empty() {
        bail!("`{key}` in Cargo.toml has an empty path");
    }
    Ok(Some(root.join(path)))
}

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

fn qualify(name: &str, table: &str) -> Result<String> {
    if name == OVERFLOW_RULE || Some(name) == OVERFLOW_RULE.strip_prefix("adamantium::") {
        return Ok(OVERFLOW_RULE.to_owned());
    }
    if EXPERIMENTAL_LINTS.contains(&name) {
        return Ok(name.to_owned());
    }
    lints::selected(true)
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
        Config::parse(&toml.parse().unwrap(), Path::new("project"))
    }

    #[test]
    fn a_manifest_without_the_table_skips_nothing() {
        let config = parse("[package]\nname = \"demo\"\n").unwrap();
        assert!(config.table.is_none());
        assert!(!config.strict);
        assert!(config.baseline.is_none());
        assert!(config.deps_baseline.is_none());
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
    fn defaults_use_the_selected_table_and_resolve_paths_from_its_manifest() {
        let package = "[package.metadata.adamantium]\n\
                       strict = false\n\
                       baseline = 'ci/package-check.json'\n\
                       deps.baseline = 'ci/package-deps.json'\n";
        let config = parse(package).unwrap();
        assert!(!config.strict);
        let root = Path::new("project");
        assert_eq!(config.baseline, Some(root.join("ci/package-check.json")));
        assert_eq!(
            config.deps_baseline,
            Some(root.join("ci/package-deps.json"))
        );

        let config = parse(&format!(
            "{package}\n\
             [workspace.metadata.adamantium]\n\
             strict = true\n\
             baseline = 'ci/workspace-check.json'\n\
             [workspace.metadata.adamantium.deps]\n\
             baseline = 'ci/workspace-deps.json'\n"
        ))
        .unwrap();
        assert!(config.strict);
        assert_eq!(config.baseline, Some(root.join("ci/workspace-check.json")));
        assert_eq!(
            config.deps_baseline,
            Some(root.join("ci/workspace-deps.json"))
        );
    }

    #[test]
    fn invalid_defaults_name_the_key_or_table() {
        for (setting, expected) in [
            (
                "strict = 'true'",
                "`package.metadata.adamantium.strict` in Cargo.toml is not a boolean",
            ),
            (
                "baseline = false",
                "`package.metadata.adamantium.baseline` in Cargo.toml is not a path string",
            ),
            (
                "baseline = ''",
                "`package.metadata.adamantium.baseline` in Cargo.toml has an empty path",
            ),
            (
                "deps = []",
                "[package.metadata.adamantium.deps] in Cargo.toml is not a table",
            ),
            (
                "deps.baseline = 3",
                "`package.metadata.adamantium.deps.baseline` in Cargo.toml is not a path string",
            ),
            (
                "deps.basline = 'ci/deps.json'",
                "unknown key `basline` in [package.metadata.adamantium.deps]; the only key is baseline",
            ),
        ] {
            let error = parse(&format!("[package.metadata.adamantium]\n{setting}"))
                .unwrap_err()
                .to_string();
            assert_eq!(error, expected);
        }
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
