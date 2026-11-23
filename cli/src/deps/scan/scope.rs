use std::{collections::HashMap, mem, slice};

use proc_macro2::Ident;
use syn::{Item, UseTree, ext::IdentExt};

use super::{super::cfg::Truth, Scanner, attrs::item_attrs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Binding {
    Item(Option<usize>, Vec<String>),
    Use {
        krate: Option<usize>,
        path: Vec<String>,
        leading: bool,
    },
    Opaque,
    Either(Vec<Binding>),
}

impl Binding {
    pub(super) fn alternatives(&self) -> &[Self] {
        match self {
            Self::Either(alternatives) => alternatives,
            other => slice::from_ref(other),
        }
    }
}

/// The single alternative, or `either` of them all when there isn't exactly one.
fn only_or<T>(alternatives: Vec<T>, either: impl FnOnce(Vec<T>) -> T) -> T {
    match <[T; 1]>::try_from(alternatives) {
        Ok([only]) => only,
        Err(alternatives) => either(alternatives),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Namespace {
    Types,
    Values,
    Macros,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Bound {
    pub(super) binding: Binding,
    pub(super) certain: bool,
}

impl Bound {
    fn add(&mut self, binding: Binding) {
        self.certain = false;
        let mut alternatives = match mem::replace(&mut self.binding, Binding::Opaque) {
            Binding::Either(alternatives) => alternatives,
            other => vec![other],
        };
        for binding in binding.alternatives() {
            if !alternatives.contains(binding) {
                alternatives.push(binding.clone());
            }
        }
        self.binding = only_or(alternatives, Binding::Either);
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Scope {
    types: HashMap<String, Bound>,
    values: HashMap<String, Bound>,
    macros: HashMap<String, Bound>,
    pub(super) globs: bool,
}

impl Scope {
    fn map(&mut self, namespace: Namespace) -> &mut HashMap<String, Bound> {
        match namespace {
            Namespace::Types => &mut self.types,
            Namespace::Values => &mut self.values,
            Namespace::Macros => &mut self.macros,
        }
    }

    pub(super) fn bound(&self, namespace: Namespace, name: &str) -> Option<&Bound> {
        let map = match namespace {
            Namespace::Types => &self.types,
            Namespace::Values => &self.values,
            Namespace::Macros => &self.macros,
        };
        map.get(name)
    }

    pub(super) fn get(&self, namespace: Namespace, name: &str) -> Option<&Binding> {
        self.bound(namespace, name).map(|bound| &bound.binding)
    }

    pub(super) fn shadow(&mut self, namespace: Namespace, name: String) {
        self.map(namespace).insert(
            name,
            Bound {
                binding: Binding::Opaque,
                certain: true,
            },
        );
    }

    pub(super) fn define(
        &mut self,
        namespace: Namespace,
        name: String,
        binding: Binding,
        certain: bool,
    ) {
        let map = self.map(namespace);
        match map.get_mut(&name) {
            Some(bound) if !(certain && bound.certain) => bound.add(binding),
            _ => {
                map.insert(name, Bound { binding, certain });
            }
        }
    }

    fn import(&mut self, name: String, binding: Binding, certain: bool) {
        for map in [&mut self.types, &mut self.values, &mut self.macros] {
            match map.get_mut(&name) {
                None => {
                    map.insert(
                        name.clone(),
                        Bound {
                            binding: binding.clone(),
                            certain,
                        },
                    );
                }
                Some(bound)
                    if certain
                        && bound.certain
                        && matches!(bound.binding, Binding::Item(..) | Binding::Opaque) => {}
                Some(bound) => bound.add(binding.clone()),
            }
        }
    }

    pub(super) fn bring_in(&mut self, name: &str, used: &Binding, item: &Binding, keep: bool) {
        let Some(bound) = self.macros.get_mut(name) else {
            return;
        };
        let mut alternatives = Vec::new();
        for alternative in bound.binding.alternatives() {
            if alternative == used {
                alternatives.push(item.clone());
                if !keep {
                    continue;
                }
            }
            if !alternatives.contains(alternative) {
                alternatives.push(alternative.clone());
            }
        }
        bound.binding = only_or(alternatives, Binding::Either);
    }

    pub(super) fn merge(&mut self, other: Self) {
        for (namespace, map) in [
            (Namespace::Types, other.types),
            (Namespace::Values, other.values),
            (Namespace::Macros, other.macros),
        ] {
            for (name, bound) in map {
                self.define(namespace, name, bound.binding, false);
            }
        }
        self.globs |= other.globs;
    }
}

pub(super) fn namespace(path: &[String], index: usize, last: Namespace) -> Namespace {
    if index + 1 == path.len() {
        last
    } else {
        Namespace::Types
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolved {
    Path {
        krate: Option<usize>,
        path: Vec<String>,
    },
    Local,
    Unknown,
    Either(Vec<Resolved>),
}

impl Resolved {
    pub(super) fn either(alternatives: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for alternative in alternatives {
            for resolved in alternative.into_alternatives() {
                if !flat.contains(&resolved) {
                    flat.push(resolved);
                }
            }
        }
        only_or(flat, Self::Either)
    }

    pub(super) fn alternatives(&self) -> &[Self] {
        match self {
            Self::Either(alternatives) => alternatives,
            other => slice::from_ref(other),
        }
    }

    pub(super) fn into_alternatives(self) -> Vec<Self> {
        match self {
            Self::Either(alternatives) => alternatives,
            other => vec![other],
        }
    }

    pub(super) fn map_paths(self, mut f: impl FnMut(Option<usize>, Vec<String>) -> Self) -> Self {
        Self::either(
            self.into_alternatives()
                .into_iter()
                .map(|resolved| match resolved {
                    Self::Path { krate, path } => f(krate, path),
                    other => other,
                }),
        )
    }

    pub(super) fn join(self, rest: &[String]) -> Self {
        self.map_paths(|krate, mut path| {
            path.extend(rest.iter().cloned());
            Self::Path { krate, path }
        })
    }
}

impl Scanner<'_> {
    fn bind_use(
        &self,
        tree: &UseTree,
        krate: Option<usize>,
        prefix: &mut Vec<String>,
        leading: bool,
        certain: bool,
        scope: &mut Scope,
    ) {
        let mut bind = |name: String, krate: Option<usize>, path: Vec<String>| {
            if name != "_" {
                scope.import(
                    name,
                    Binding::Use {
                        krate,
                        path,
                        leading,
                    },
                    certain,
                );
            }
        };
        let dollar = |ident: &Ident| {
            (krate.is_none() && prefix.is_empty() && !leading)
                .then(|| self.dollar_crate(ident))
                .flatten()
        };
        match tree {
            UseTree::Path(tree) => {
                if let Some(id) = dollar(&tree.ident)
                    && let Some(krate) = self.crates.get(id)
                {
                    prefix.push(krate.name.to_owned());
                    self.bind_use(&tree.tree, Some(id), prefix, leading, certain, scope);
                } else {
                    prefix.push(tree.ident.unraw().to_string());
                    self.bind_use(&tree.tree, krate, prefix, leading, certain, scope);
                }
                prefix.pop();
            }
            UseTree::Name(name) => {
                if let Some(id) = dollar(&name.ident)
                    && let Some(krate) = self.crates.get(id)
                {
                    let lib = krate.name.to_owned();
                    bind(lib.clone(), Some(id), vec![lib]);
                } else {
                    let ident = name.ident.unraw().to_string();
                    if ident == "self" {
                        if let Some(last) = prefix.last() {
                            bind(last.clone(), krate, prefix.clone());
                        }
                    } else {
                        let mut path = prefix.clone();
                        path.push(ident.clone());
                        bind(ident, krate, path);
                    }
                }
            }
            UseTree::Rename(rename) => {
                if let Some(id) = dollar(&rename.ident)
                    && let Some(krate) = self.crates.get(id)
                {
                    let lib = krate.name.to_owned();
                    bind(rename.rename.unraw().to_string(), Some(id), vec![lib]);
                } else {
                    let ident = rename.ident.unraw().to_string();
                    let mut path = prefix.clone();
                    if ident != "self" {
                        path.push(ident);
                    }
                    bind(rename.rename.unraw().to_string(), krate, path);
                }
            }
            UseTree::Glob(_) => scope.globs = true,
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.bind_use(tree, krate, prefix, leading, certain, scope);
                }
            }
        }
    }
}

impl<'a> Scanner<'a> {
    pub(super) fn bind<'i>(
        &self,
        scope: &mut Scope,
        items: impl IntoIterator<Item = &'i Item>,
        module: Option<&[String]>,
        uncertain: bool,
    ) {
        for item in items {
            let certain = match self.krate.cfg.enabled(item_attrs(item)) {
                Truth::True => !uncertain,
                Truth::Unknown => false,
                Truth::False => continue,
            };
            let ident = match item {
                Item::Use(item) => {
                    let leading = item.leading_colon.is_some();
                    self.bind_use(&item.tree, None, &mut Vec::new(), leading, certain, scope);
                    continue;
                }
                Item::ExternCrate(item) => {
                    let name = item
                        .rename
                        .as_ref()
                        .map_or(&item.ident, |(_, rename)| rename);
                    let target = item.ident.unraw().to_string();
                    let (krate, target) = if target == "self" {
                        (None, self.krate.name.to_owned())
                    } else {
                        self.extern_crate(&target).unwrap_or((None, target))
                    };
                    scope.define(
                        Namespace::Types,
                        name.unraw().to_string(),
                        Binding::Item(krate, vec![target]),
                        certain,
                    );
                    continue;
                }
                Item::Const(item) => (&item.ident, &[Namespace::Values][..]),
                Item::Enum(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Fn(item) => (&item.sig.ident, &[Namespace::Values][..]),
                Item::Mod(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Static(item) => (&item.ident, &[Namespace::Values][..]),
                Item::Struct(item) => (&item.ident, &[Namespace::Types, Namespace::Values][..]),
                Item::Trait(item) => (&item.ident, &[Namespace::Types][..]),
                Item::TraitAlias(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Type(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Union(item) => (&item.ident, &[Namespace::Types][..]),
                _ => continue,
            };
            let (ident, namespaces) = ident;
            let name = ident.unraw().to_string();
            let binding = match module {
                Some(module) => {
                    let mut path = module.to_vec();
                    path.push(name.clone());
                    Binding::Item(None, path)
                }
                None => Binding::Opaque,
            };
            for namespace in namespaces {
                scope.define(*namespace, name.clone(), binding.clone(), certain);
            }
        }
    }
}
