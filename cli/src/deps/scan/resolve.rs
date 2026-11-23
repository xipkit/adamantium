use std::collections::HashMap;

use proc_macro2::Ident;

use super::{
    Scanner, SpanStart,
    scope::{Binding, Namespace, Resolved, Scope, namespace},
    visitor::segments,
};

pub(super) const MAX_DEPTH: usize = 16;

const PRELUDE: [(&str, &str); 37] = [
    ("Box", "std::boxed::Box"),
    ("Vec", "std::vec::Vec"),
    ("String", "std::string::String"),
    ("Option", "std::option::Option"),
    ("Some", "std::option::Option::Some"),
    ("None", "std::option::Option::None"),
    ("Result", "std::result::Result"),
    ("Ok", "std::result::Result::Ok"),
    ("Err", "std::result::Result::Err"),
    ("drop", "std::mem::drop"),
    ("ToOwned", "std::borrow::ToOwned"),
    ("ToString", "std::string::ToString"),
    ("Clone", "std::clone::Clone"),
    ("Copy", "std::marker::Copy"),
    ("Send", "std::marker::Send"),
    ("Sync", "std::marker::Sync"),
    ("Sized", "std::marker::Sized"),
    ("Unpin", "std::marker::Unpin"),
    ("Drop", "std::ops::Drop"),
    ("Fn", "std::ops::Fn"),
    ("FnMut", "std::ops::FnMut"),
    ("FnOnce", "std::ops::FnOnce"),
    ("AsRef", "std::convert::AsRef"),
    ("AsMut", "std::convert::AsMut"),
    ("From", "std::convert::From"),
    ("Into", "std::convert::Into"),
    ("TryFrom", "std::convert::TryFrom"),
    ("TryInto", "std::convert::TryInto"),
    ("Default", "std::default::Default"),
    ("PartialEq", "std::cmp::PartialEq"),
    ("Eq", "std::cmp::Eq"),
    ("PartialOrd", "std::cmp::PartialOrd"),
    ("Ord", "std::cmp::Ord"),
    ("Iterator", "std::iter::Iterator"),
    ("IntoIterator", "std::iter::IntoIterator"),
    ("Extend", "std::iter::Extend"),
    ("FromIterator", "std::iter::FromIterator"),
];

const SYSROOT: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

pub(super) struct Resolver<'s, 'a> {
    scanner: &'s Scanner<'a>,
    indexing: Option<(&'s [String], &'s Scope)>,
    blocks: &'s [Scope],
    uses: HashMap<UseKey, Resolved>,
    walks: HashMap<WalkKey, Resolved>,
    active: Vec<UseSite>,
    circular: Option<usize>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct UseSite {
    module: Vec<String>,
    blocks: usize,
    namespace: Namespace,
    path: Vec<String>,
    leading: bool,
}

#[derive(PartialEq, Eq, Hash)]
struct UseKey {
    site: UseSite,
    depth: usize,
}

#[derive(PartialEq, Eq, Hash)]
struct WalkKey {
    path: Vec<String>,
    start: usize,
    namespace: Namespace,
    depth: usize,
}

impl<'s, 'a> Resolver<'s, 'a> {
    pub(super) fn new(scanner: &'s Scanner<'a>, blocks: &'s [Scope]) -> Self {
        Self {
            scanner,
            indexing: None,
            blocks,
            uses: HashMap::new(),
            walks: HashMap::new(),
            active: Vec::new(),
            circular: None,
        }
    }

    pub(super) fn indexing(mut self, module: &'s [String], scope: &'s Scope) -> Self {
        self.indexing = Some((module, scope));
        self
    }

    pub(super) fn module(&self, module: &[String]) -> Option<&'s Scope> {
        if let Some(scope) = self.scanner.last_pass.get(module) {
            return Some(scope);
        }
        match self.indexing {
            Some((indexing, scope)) if indexing == module => Some(scope),
            _ => {
                let scanner = self.scanner;
                scanner
                    .around
                    .iter()
                    .rev()
                    .find(|(around, _)| around == module)
                    .map(|(_, scope)| scope)
                    .or_else(|| scanner.modules.get(module))
            }
        }
    }

    fn guarded(
        &mut self,
        outside: usize,
        resolve: impl FnOnce(&mut Self) -> Resolved,
    ) -> (Resolved, bool) {
        let before = self.circular.take();
        let resolved = resolve(self);
        let reached = self.circular.filter(|index| *index < outside);
        self.circular = before.into_iter().chain(reached).min();
        (resolved, reached.is_none())
    }

    pub(super) fn syn_path(
        &mut self,
        module: &[String],
        namespace: Namespace,
        path: &syn::Path,
    ) -> Resolved {
        let names = segments(path);
        let leading = path.leading_colon.is_some();
        if !leading
            && let Some(first) = path.segments.first()
            && let Some(id) = self.scanner.dollar_crate(&first.ident)
            && let Some(krate) = self.scanner.crates.get(id)
        {
            let mut full = vec![krate.name.to_owned()];
            full.extend(names.into_iter().skip(1));
            return self.follow(Some(id), full, namespace, 0);
        }
        self.path(module, namespace, &names, leading)
    }

    pub(super) fn path(
        &mut self,
        module: &[String],
        namespace: Namespace,
        path: &[String],
        leading: bool,
    ) -> Resolved {
        match self.resolve_path(module, namespace, path, leading) {
            Resolved::Either(alternatives) if alternatives.is_empty() => Resolved::Unknown,
            resolved => resolved,
        }
    }

    fn resolve_path(
        &mut self,
        module: &[String],
        namespace: Namespace,
        path: &[String],
        leading: bool,
    ) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        if first == "self" && rest.is_empty() {
            return Resolved::Local;
        }
        let scanner = self.scanner;
        if let Some(path) = scanner.relative_path(module, path) {
            return self.follow(None, path, namespace, 0);
        }
        if leading {
            if scanner.krate.edition_2015 {
                return self.root_path(path, namespace, 0);
            }
            return scanner
                .extern_path(first, rest)
                .unwrap_or(Resolved::Unknown);
        }
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some((binding, scope)) =
            self.lookup(module, self.blocks.len(), first_namespace, first)
        {
            let resolved = self
                .binding(module, scope, first_namespace, binding, 0)
                .join(rest);
            return self.followed(resolved, namespace, 0);
        }
        if let Some(resolved) = scanner.extern_path(first, rest) {
            return resolved;
        }
        let globs = self
            .blocks
            .iter()
            .chain(self.module(module))
            .any(|scope| scope.globs);
        if !globs && let Some((_, prelude)) = PRELUDE.iter().find(|(name, _)| name == first) {
            return Resolved::Path {
                krate: None,
                path: prelude.split("::").map(str::to_owned).collect(),
            }
            .join(rest);
        }
        Resolved::Unknown
    }

    fn lookup(
        &self,
        module: &[String],
        blocks: usize,
        namespace: Namespace,
        name: &str,
    ) -> Option<(&'s Binding, usize)> {
        let around = self.blocks.get(..blocks).unwrap_or_default();
        for (index, scope) in around.iter().enumerate().rev() {
            if let Some(binding) = scope.get(namespace, name) {
                return Some((binding, index + 1));
            }
        }
        self.module(module)
            .and_then(|scope| scope.get(namespace, name))
            .map(|binding| (binding, 0))
    }

    pub(super) fn binding(
        &mut self,
        module: &[String],
        blocks: usize,
        namespace: Namespace,
        binding: &Binding,
        depth: usize,
    ) -> Resolved {
        match binding {
            Binding::Item(krate, path) => Resolved::Path {
                krate: *krate,
                path: path.clone(),
            },
            Binding::Opaque => Resolved::Local,
            Binding::Use {
                krate: Some(id),
                path,
                ..
            } => Resolved::Path {
                krate: Some(*id),
                path: path.clone(),
            },
            Binding::Use {
                krate: None,
                path,
                leading,
            } => self.use_path(module, blocks, namespace, path, *leading, depth + 1),
            Binding::Either(alternatives) => Resolved::either(
                alternatives
                    .iter()
                    .map(|alternative| self.binding(module, blocks, namespace, alternative, depth)),
            ),
        }
    }

    fn use_path(
        &mut self,
        module: &[String],
        blocks: usize,
        namespace: Namespace,
        path: &[String],
        leading: bool,
        depth: usize,
    ) -> Resolved {
        let key = UseKey {
            site: UseSite {
                module: module.to_vec(),
                blocks,
                namespace,
                path: path.to_vec(),
                leading,
            },
            depth,
        };
        if let Some(resolved) = self.uses.get(&key) {
            return resolved.clone();
        }
        if let Some(index) = self.active.iter().position(|site| *site == key.site) {
            self.circular = Some(self.circular.map_or(index, |circular| circular.min(index)));
            return Resolved::Either(Vec::new());
        }
        let outside = self.active.len();
        self.active.push(key.site.clone());
        let (resolved, holds) = self.guarded(outside, |resolver| {
            resolver.resolve_use(module, blocks, namespace, path, leading, depth)
        });
        self.active.pop();
        if holds {
            self.uses.insert(key, resolved.clone());
        }
        resolved
    }

    fn resolve_use(
        &mut self,
        module: &[String],
        blocks: usize,
        namespace: Namespace,
        path: &[String],
        leading: bool,
        depth: usize,
    ) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        if depth > MAX_DEPTH {
            return Resolved::Unknown;
        }
        let scanner = self.scanner;
        if let Some(path) = scanner.relative_path(module, path) {
            return self.follow(None, path, namespace, depth);
        }
        if scanner.krate.edition_2015 {
            return self.root_path(path, namespace, depth);
        }
        let unbound = |_: &mut Self| {
            scanner
                .extern_path(first, rest)
                .unwrap_or(Resolved::Unknown)
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if !leading
            && let Some((binding, scope)) = self.lookup(module, blocks, first_namespace, first)
        {
            return self.use_binding(module, scope, binding, path, namespace, depth, unbound);
        }
        unbound(self)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "each is a part of the place being resolved"
    )]
    fn use_binding(
        &mut self,
        module: &[String],
        blocks: usize,
        binding: &Binding,
        path: &[String],
        namespace: Namespace,
        depth: usize,
        unbound: impl FnOnce(&mut Self) -> Resolved,
    ) -> Resolved {
        let first_namespace = self::namespace(path, 0, namespace);
        let rest = path.get(1..).unwrap_or_default();
        let mut alternatives = Vec::new();
        let mut itself = false;
        for alternative in binding.alternatives() {
            if is_use_of(alternative, path) {
                itself = true;
                continue;
            }
            let resolved = self
                .binding(module, blocks, first_namespace, alternative, depth)
                .join(rest);
            alternatives.push(self.followed(resolved, namespace, depth));
        }
        if itself {
            alternatives.push(unbound(self));
        }
        Resolved::either(alternatives)
    }

    fn root_path(&mut self, path: &[String], namespace: Namespace, depth: usize) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        let scanner = self.scanner;
        let root = [scanner.krate.name.to_owned()];
        let unbound = |resolver: &mut Self| {
            if let Some(resolved) = scanner.extern_path(first, rest) {
                return resolved;
            }
            let mut full = root.to_vec();
            full.extend(path.iter().cloned());
            resolver.follow(None, full, namespace, depth)
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some((binding, _)) = self.lookup(&root, 0, first_namespace, first) {
            return self.use_binding(&root, 0, binding, path, namespace, depth, unbound);
        }
        unbound(self)
    }

    fn followed(&mut self, resolved: Resolved, namespace: Namespace, depth: usize) -> Resolved {
        resolved.map_paths(|krate, path| self.follow(krate, path, namespace, depth))
    }

    fn follow(
        &mut self,
        krate: Option<usize>,
        path: Vec<String>,
        namespace: Namespace,
        depth: usize,
    ) -> Resolved {
        if krate.is_some_and(|id| id != self.scanner.id) {
            return Resolved::Path { krate, path };
        }
        if self.scanner.disallowed.entry(&path).is_some() {
            return Resolved::Path { krate, path };
        }
        self.step(path, namespace, depth)
    }

    pub(super) fn step(
        &mut self,
        path: Vec<String>,
        namespace: Namespace,
        depth: usize,
    ) -> Resolved {
        self.walk(path, 1, namespace, depth)
    }

    fn walk(
        &mut self,
        path: Vec<String>,
        start: usize,
        namespace: Namespace,
        depth: usize,
    ) -> Resolved {
        let key = WalkKey {
            path,
            start,
            namespace,
            depth,
        };
        if let Some(resolved) = self.walks.get(&key) {
            return resolved.clone();
        }
        let (resolved, holds) = self.guarded(self.active.len(), |resolver| {
            resolver.resolve_walk(&key.path, start, namespace, depth)
        });
        if holds {
            self.walks.insert(key, resolved.clone());
        }
        resolved
    }

    fn resolve_walk(
        &mut self,
        path: &[String],
        start: usize,
        namespace: Namespace,
        depth: usize,
    ) -> Resolved {
        let scanner = self.scanner;
        if path.first().map(String::as_str) != Some(scanner.krate.name) || depth > MAX_DEPTH {
            return Resolved::Path {
                krate: None,
                path: path.to_vec(),
            };
        }
        for end in start..path.len() {
            let (Some(module), Some(name)) = (path.get(..end), path.get(end)) else {
                break;
            };
            let Some(scope) = self.module(module) else {
                break;
            };
            let name_namespace = self::namespace(path, end, namespace);
            match scope.get(name_namespace, name) {
                Some(binding @ (Binding::Use { .. } | Binding::Either(_))) => {
                    let rest = path.get(end + 1..).unwrap_or_default();
                    return Resolved::either(binding.alternatives().iter().map(|alternative| {
                        if let Binding::Use { .. } = alternative {
                            let resolved = self
                                .binding(module, 0, name_namespace, alternative, depth + 1)
                                .join(rest);
                            self.followed(resolved, namespace, depth + 1)
                        } else {
                            self.walk(path.to_vec(), end + 1, namespace, depth + 1)
                        }
                    }));
                }
                Some(_) => {}
                None if scope.globs => return Resolved::Unknown,
                None => break,
            }
        }
        Resolved::Path {
            krate: None,
            path: path.to_vec(),
        }
    }
}

fn is_use_of(binding: &Binding, path: &[String]) -> bool {
    matches!(binding, Binding::Use { krate: None, path: used, .. } if used == path)
}

impl<'a> Scanner<'a> {
    pub(super) fn extern_crate(&self, name: &str) -> Option<(Option<usize>, String)> {
        if let Some(lib) = self.krate.externs.get(name) {
            let id = self.krate.dependencies.get(name).copied();
            return Some((id, lib.clone()));
        }
        SYSROOT.contains(&name).then(|| (None, name.to_owned()))
    }

    fn extern_path(&self, first: &str, rest: &[String]) -> Option<Resolved> {
        let (krate, lib) = self.extern_crate(first)?;
        let root = Resolved::Path {
            krate,
            path: vec![lib],
        };
        Some(root.join(rest))
    }

    pub(super) fn dollar_crate(&self, ident: &Ident) -> Option<usize> {
        if ident != "crate" {
            return None;
        }
        self.dollar_crates
            .get(&SpanStart::of(ident.span()))
            .copied()
    }

    fn relative_path(&self, module: &[String], path: &[String]) -> Option<Vec<String>> {
        let mut base: Option<Vec<String>> = None;
        let mut rest = path;
        while let Some((first, tail)) = rest.split_first() {
            match first.as_str() {
                "crate" if base.is_none() => base = Some(vec![self.krate.name.to_owned()]),
                "self" if base.is_none() => base = Some(module.to_vec()),
                "super" => {
                    let parent = base.get_or_insert_with(|| module.to_vec());
                    if parent.len() > 1 {
                        parent.pop();
                    }
                }
                _ => break,
            }
            rest = tail;
        }
        base.map(|mut base| {
            base.extend(rest.iter().cloned());
            base
        })
    }
}
