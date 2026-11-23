use std::collections::{BTreeSet, HashMap};

use syn::{Item, PathArguments, UseTree, visit::Visit};

use super::{Walker, locks::is_unsized};
use crate::fix::source::item_attributes;

#[derive(Default)]
pub(super) struct Names {
    pub(super) constants: BTreeSet<String>,
    pub(super) macros: BTreeSet<String>,
    pub(super) unsized_aliases: BTreeSet<String>,
    /// How many items and imports in the file declare each name.
    pub(super) declared: HashMap<String, usize>,
}

impl<'ast> Visit<'ast> for Names {
    fn visit_item(&mut self, item: &'ast Item) {
        let bindings = ItemScope::bindings(item);
        for (name, _) in &bindings {
            *self.declared.entry(name.clone()).or_default() += 1;
        }
        match item {
            Item::Const(item) => {
                self.constants.insert(item.ident.to_string());
            }
            Item::Static(item) => {
                self.constants.insert(item.ident.to_string());
            }
            Item::Struct(item) => {
                self.constants.insert(item.ident.to_string());
            }
            Item::Type(item) if is_unsized(&item.ty, &BTreeSet::new()) => {
                self.unsized_aliases.insert(item.ident.to_string());
            }
            Item::Use(_) => {
                for (name, _) in bindings {
                    self.constants.insert(name.clone());
                    self.macros.insert(name);
                }
            }
            Item::Macro(mac) => {
                if let Some(name) = &mac.ident {
                    self.macros.insert(name.to_string());
                }
            }
            _ => {}
        }
        syn::visit::visit_item(self, item);
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Binding {
    Import(Vec<String>),
    Item,
    Parameter,
    Ambiguous,
}

pub(super) struct ItemScope {
    pub(super) names: HashMap<String, Binding>,
    globs: Vec<Vec<String>>,
    module: bool,
}

impl ItemScope {
    pub(super) fn new<'i>(items: impl IntoIterator<Item = &'i Item>, module: bool) -> Self {
        let mut scope = Self {
            names: HashMap::new(),
            globs: Vec::new(),
            module,
        };
        for item in items {
            let mut own = Self {
                names: HashMap::new(),
                globs: Vec::new(),
                module,
            };
            if let Item::Use(item) = item {
                let mut prefix = item
                    .leading_colon
                    .map(|_| vec!["::".to_owned()])
                    .unwrap_or_default();
                own.import(&item.tree, &mut prefix);
            } else {
                own.names.extend(Self::bindings(item));
            }
            scope.globs.extend(own.globs);
            let conditional = item_attributes(item)
                .iter()
                .any(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"));
            for (name, binding) in own.names {
                let binding = match scope.names.get(&name) {
                    _ if conditional => Binding::Ambiguous,
                    Some(existing) if *existing != binding => Binding::Ambiguous,
                    _ => binding,
                };
                scope.names.insert(name, binding);
            }
        }
        scope
    }

    fn import(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.import(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) if name.ident == "self" => {
                if let Some(last) = prefix.last() {
                    self.names
                        .insert(last.clone(), Binding::Import(prefix.clone()));
                }
            }
            UseTree::Name(name) => {
                let mut path = prefix.clone();
                path.push(name.ident.to_string());
                self.names
                    .insert(name.ident.to_string(), Binding::Import(path));
            }
            UseTree::Rename(rename) => {
                let mut path = prefix.clone();
                path.push(rename.ident.to_string());
                self.names
                    .insert(rename.rename.to_string(), Binding::Import(path));
            }
            UseTree::Glob(_) => self.globs.push(prefix.clone()),
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.import(tree, prefix);
                }
            }
        }
    }

    fn bindings(item: &Item) -> Vec<(String, Binding)> {
        let item_name = |ident: &syn::Ident| vec![(ident.to_string(), Binding::Item)];
        match item {
            Item::Const(item) => item_name(&item.ident),
            Item::Enum(item) => item_name(&item.ident),
            Item::ExternCrate(item) => {
                let name = item.rename.as_ref().map_or(&item.ident, |(_, name)| name);
                vec![(
                    name.to_string(),
                    Binding::Import(vec!["::".to_owned(), item.ident.to_string()]),
                )]
            }
            Item::Fn(item) => item_name(&item.sig.ident),
            Item::Mod(item) => item_name(&item.ident),
            Item::Static(item) => item_name(&item.ident),
            Item::Struct(item) => item_name(&item.ident),
            Item::Trait(item) => item_name(&item.ident),
            Item::TraitAlias(item) => item_name(&item.ident),
            Item::Type(item) => item_name(&item.ident),
            Item::Union(item) => item_name(&item.ident),
            Item::Use(item) => {
                let mut scope = Self {
                    names: HashMap::new(),
                    globs: Vec::new(),
                    module: false,
                };
                scope.import(&item.tree, &mut Vec::new());
                scope.names.into_iter().collect()
            }
            _ => Vec::new(),
        }
    }
}

pub(super) enum Resolved {
    Path(Vec<String>),
    Item,
    Parameter,
    Prelude,
    Ambiguous,
}

impl From<&Binding> for Resolved {
    fn from(binding: &Binding) -> Self {
        match binding {
            Binding::Import(path) => Self::Path(path.clone()),
            Binding::Item => Self::Item,
            Binding::Parameter => Self::Parameter,
            Binding::Ambiguous => Self::Ambiguous,
        }
    }
}

pub(super) const ARC_CRATES: &[&str] = &["std", "alloc"];
pub(super) const ARC: &[&str] = &["sync", "Arc"];

/// The names of the first `segments` segments of `path`, when none of them has
/// generic arguments.
pub(super) fn plain_names(path: &syn::Path, segments: usize) -> Option<Vec<String>> {
    path.segments
        .iter()
        .take(segments)
        .map(|segment| {
            matches!(segment.arguments, PathArguments::None).then(|| segment.ident.to_string())
        })
        .collect()
}

impl<'a> Walker<'a> {
    pub(super) fn resolve_name(&self, name: &str) -> Resolved {
        let mut scopes = self.items.iter().rev();
        while let Some(scope) = scopes.next() {
            if let Some(binding) = scope.names.get(name) {
                return binding.into();
            }
            if scope.module {
                let parent = scopes.clone().find(|scope| scope.module);
                let only_super = scope.globs.iter().all(|glob| glob == &["super"]);
                if !scope.globs.is_empty() {
                    return match parent {
                        Some(parent) if only_super && parent.globs.is_empty() => parent
                            .names
                            .get(name)
                            .map_or(Resolved::Prelude, Resolved::from),
                        _ => Resolved::Ambiguous,
                    };
                }
                return Resolved::Prelude;
            }
            if !scope.globs.is_empty() {
                return Resolved::Ambiguous;
            }
        }
        Resolved::Prelude
    }

    pub(super) fn resolve_path(&self, path: &syn::Path, segments: usize) -> Option<Vec<String>> {
        self.resolve_names(path.leading_colon.is_some(), plain_names(path, segments)?)
    }

    /// Resolves a path given as its segment names; an `absolute` one starts
    /// with `::`.
    pub(super) fn resolve_names(&self, absolute: bool, names: Vec<String>) -> Option<Vec<String>> {
        if absolute {
            return Some(std::iter::once("::".to_owned()).chain(names).collect());
        }
        let (first, rest) = names.split_first()?;
        let mut resolved = match self.resolve_name(first) {
            Resolved::Path(path) => path,
            Resolved::Prelude => vec![first.clone()],
            Resolved::Item | Resolved::Parameter | Resolved::Ambiguous => return None,
        };
        resolved.extend(rest.iter().cloned());
        Some(resolved)
    }

    pub(super) fn is_arc(&self, path: &syn::Path, skip: usize) -> bool {
        let Some(count) = path.segments.len().checked_sub(skip) else {
            return false;
        };
        self.names_item(self.resolve_path(path, count), ARC_CRATES, ARC)
    }

    /// Whether `resolved` is the item `tail` of one of the crates `roots`.
    pub(super) fn names_item(
        &self,
        resolved: Option<Vec<String>>,
        roots: &[&str],
        tail: &[&str],
    ) -> bool {
        resolved
            .and_then(|resolved| self.crate_path(resolved))
            .is_some_and(|resolved| {
                resolved.split_first().is_some_and(|(root, rest)| {
                    roots.contains(&root.as_str())
                        && rest.iter().map(String::as_str).eq(tail.iter().copied())
                })
            })
    }

    fn crate_path(&self, resolved: Vec<String>) -> Option<Vec<String>> {
        if resolved.first().is_some_and(|first| first == "::") {
            return Some(resolved.into_iter().skip(1).collect());
        }
        (!resolved
            .first()
            .is_some_and(|first| self.names.declared.contains_key(first)))
        .then_some(resolved)
    }
}
