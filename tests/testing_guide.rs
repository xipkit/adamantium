//! Keeps `docs/testing.md` naming every public item of `adamantium::testing`.
#![cfg(feature = "deterministic")]

use std::{collections::BTreeSet, error::Error, fs, path::Path};

use syn::{Item, UseTree, Visibility};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn read(path: &str) -> Result<String> {
    Ok(fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path),
    )?)
}

/// The names the module makes public: its `pub` items and the names its
/// `pub use` declarations re-export.
fn exports(source: &str) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for item in syn::parse_file(source)?.items {
        let (vis, ident) = match &item {
            Item::Const(item) => (&item.vis, &item.ident),
            Item::Enum(item) => (&item.vis, &item.ident),
            Item::Fn(item) => (&item.vis, &item.sig.ident),
            Item::Mod(item) => (&item.vis, &item.ident),
            Item::Static(item) => (&item.vis, &item.ident),
            Item::Struct(item) => (&item.vis, &item.ident),
            Item::Trait(item) => (&item.vis, &item.ident),
            Item::TraitAlias(item) => (&item.vis, &item.ident),
            Item::Type(item) => (&item.vis, &item.ident),
            Item::Union(item) => (&item.vis, &item.ident),
            Item::Use(item) => {
                if matches!(item.vis, Visibility::Public(_)) {
                    reexports(&item.tree, &mut names)?;
                }
                continue;
            }
            _ => continue,
        };
        if matches!(vis, Visibility::Public(_)) {
            names.insert(ident.to_string());
        }
    }
    Ok(names)
}

fn reexports(tree: &UseTree, names: &mut BTreeSet<String>) -> Result<()> {
    match tree {
        UseTree::Path(path) => reexports(&path.tree, names)?,
        UseTree::Name(name) => {
            names.insert(name.ident.to_string());
        }
        UseTree::Rename(rename) => {
            names.insert(rename.rename.to_string());
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                reexports(tree, names)?;
            }
        }
        UseTree::Glob(_) => {
            return Err("a glob re-export hides which names are public; list them".into());
        }
    }
    Ok(())
}

/// Every identifier in the guide's code: fenced blocks and inline code spans.
fn code_identifiers(markdown: &str) -> BTreeSet<String> {
    let mut code = String::new();
    let mut prose = String::new();
    let mut fenced = false;
    for line in markdown.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            code.push_str(line);
            code.push('\n');
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    // Inline code spans are the text between pairs of backticks, and can
    // continue onto the next line.
    for span in prose.split('`').skip(1).step_by(2) {
        code.push_str(span);
        code.push('\n');
    }
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_testing_guide_names_every_public_item_of_the_testing_module() -> Result<()> {
    let exports = exports(&read("src/testing.rs")?)?;
    for expected in ["check", "Config", "FaultInjector", "duplex_controlled"] {
        assert!(
            exports.contains(expected),
            "{expected} missing from {exports:?}"
        );
    }
    let named = code_identifiers(&read("docs/testing.md")?);
    let missing: Vec<_> = exports.difference(&named).collect();
    assert!(
        missing.is_empty(),
        "docs/testing.md never names {missing:?}"
    );
    Ok(())
}

#[test]
fn exports_are_public_items_and_reexported_names() -> Result<()> {
    let source = "
        mod private;
        pub mod public;
        pub use private::{Kept, nested::{Deep, Old as New}};
        pub(crate) use private::Internal;
        pub struct Shown;
        pub(crate) struct Crate;
        struct Hidden;
        pub async fn run() {}
        fn helper() {}
        impl Shown { pub fn method(&self) {} }
    ";
    let expected = ["Deep", "Kept", "New", "Shown", "public", "run"];
    assert_eq!(exports(source)?, expected.map(String::from).into());
    assert!(exports("pub use private::*;").is_err());
    Ok(())
}

#[test]
fn only_code_counts_as_naming_an_item() {
    let markdown = "Call `check` or\n`testing::\nreplay`, not shrink.\n\n```rust\nlet record = Replay::load(path);\n```\nThen explain.\n";
    let expected = [
        "Replay", "check", "let", "load", "path", "record", "replay", "testing",
    ];
    assert_eq!(
        code_identifiers(markdown),
        expected.map(String::from).into()
    );
}
