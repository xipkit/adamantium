//! Reading and editing a project's `Cargo.toml`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{
    config::Config,
    lints::{CLIPPY_TOML, LINTS, OVERFLOW_RULE, Tool},
};

/// The directory holding a project's `Cargo.toml`, from a path to either.
pub fn root(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    let dir = if path.is_file() {
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?
    } else {
        path
    };
    if !dir.join("Cargo.toml").is_file() {
        bail!("no Cargo.toml in {}", dir.display());
    }
    Ok(dir)
}

fn read(root: &Path) -> Result<DocumentMut> {
    let path = root.join("Cargo.toml");
    fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Whether the release profile checks for integer overflow, so that overflow
/// panics in release builds as it does in debug builds.
pub fn release_checks_overflow(root: &Path) -> Result<bool> {
    let doc = read(root)?;
    Ok(doc
        .get("profile")
        .and_then(|profile| profile.get("release"))
        .and_then(|release| release.get("overflow-checks"))
        .and_then(Item::as_bool)
        .unwrap_or(false))
}

/// What `init` changed, for the user to read.
#[derive(Debug, Default)]
pub struct Changes {
    pub notes: Vec<String>,
}

/// Writes the rules into a project: the lint tables and the release
/// profile's overflow check into `Cargo.toml`, and `clippy.toml` beside it.
/// Rules the skip list allows or excepts are left out of the tables, since a
/// `[lints]` table cannot scope a rule to a path. Running it again changes
/// nothing.
pub fn init(root: &Path, force: bool, config: &Config) -> Result<Changes> {
    let mut changes = Changes::default();
    let manifest_path = root.join("Cargo.toml");
    let before = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut doc: DocumentMut = before
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let is_workspace = doc.contains_key("workspace");
    let has_package = doc.contains_key("package");
    if !is_workspace && !has_package {
        bail!(
            "{} has neither [package] nor [workspace]",
            manifest_path.display()
        );
    }

    let members: Vec<String> = doc
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(Item::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|member| member.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let document = doc.as_table_mut();
    let lints_parent = if is_workspace {
        table_in(document, "workspace", false)?
    } else {
        document
    };
    let lints = table_in(lints_parent, "lints", true)?;
    if lints.get("workspace").and_then(Item::as_bool) == Some(true) {
        bail!(
            "{} inherits its lints from a workspace; run `cargo adamantium init` on the workspace root",
            manifest_path.display()
        );
    }
    for tool in [Tool::Rust, Tool::Clippy] {
        let name = tool.to_string();
        let table = table_in(lints, &name, false)?;
        for lint in LINTS.iter().filter(|lint| lint.tool == tool) {
            if config.adopted(&lint.qualified()) {
                table.insert(lint.name, value(lint.level.to_string()));
            } else {
                table.remove(lint.name);
            }
        }
        if table.is_empty() {
            lints.remove(&name);
        }
    }

    let document = doc.as_table_mut();
    if is_workspace && has_package {
        let package_lints = table_in(document, "lints", false)?;
        if package_lints.is_empty() {
            package_lints.insert("workspace", value(true));
        } else if package_lints.get("workspace").and_then(Item::as_bool) != Some(true) {
            changes.notes.push(
                "the root package has its own [lints] table; add `workspace = true` to it or remove it".to_owned(),
            );
        }
    }
    if !members.is_empty() {
        changes.notes.push(format!(
            "each workspace member needs `[lints]\\nworkspace = true` in its Cargo.toml: {}",
            members.join(", ")
        ));
    }

    if config.adopted(OVERFLOW_RULE) {
        let profile = table_in(document, "profile", true)?;
        let release = table_in(profile, "release", false)?;
        release.insert("overflow-checks", value(true));
    }

    let after = doc.to_string();
    if after != before {
        fs::write(&manifest_path, after)
            .with_context(|| format!("writing {}", manifest_path.display()))?;
        changes
            .notes
            .push(format!("wrote the rules into {}", manifest_path.display()));
        if config.skips_paths()
            && let Some(table) = &config.table
        {
            changes.notes.push(format!(
                "the paths in [{table}].skip only apply to `cargo adamantium check`; plain `cargo clippy` checks them too"
            ));
        }
    }

    let clippy_path = root.join("clippy.toml");
    match fs::read_to_string(&clippy_path) {
        Ok(existing) if existing == CLIPPY_TOML => {}
        Ok(_) if !force => bail!(
            "{} exists and differs from adamantium's; pass --force to replace it",
            clippy_path.display()
        ),
        _ => {
            fs::write(&clippy_path, CLIPPY_TOML)
                .with_context(|| format!("writing {}", clippy_path.display()))?;
            changes
                .notes
                .push(format!("wrote {}", clippy_path.display()));
        }
    }
    Ok(changes)
}

/// The table at `key` in `parent`, created if missing. An implicit table
/// prints no header of its own when all it holds is other tables.
fn table_in<'a>(parent: &'a mut Table, key: &str, implicit: bool) -> Result<&'a mut Table> {
    let item = parent.entry(key).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(implicit);
        Item::Table(table)
    });
    item.as_table_mut()
        .ok_or_else(|| anyhow!("`{key}` in Cargo.toml is not a table"))
}
