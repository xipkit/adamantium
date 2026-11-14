//! Reading one dependency's library with syn: its `unsafe` code, and its
//! uses of the paths `clippy.toml` disallows.
//!
//! The scan walks the module tree from the library's root, as the compiler
//! does, so tests, examples, benches and files no module declares are left
//! out. syn sees syntax, not resolved names, so paths are resolved as far as
//! the crate's source shows them: through each module's `use` and `extern
//! crate` items, `crate`, `self` and `super`, re-exports between the crate's
//! modules, its dependencies' names and the standard prelude. A call that
//! this can't tie to a path, such as a method call on a value, is counted as
//! unresolved under its name rather than guessed at.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, mem,
    path::{Component, Path, PathBuf},
    slice,
};

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use serde::Serialize;
use syn::{
    Arm, Attribute, Block, Expr, ExprCall, ExprClosure, ExprForLoop, ExprIf, ExprLit,
    ExprMethodCall, ExprPath, ExprUnsafe, ExprWhile, Field, FieldValue, File, FnArg, GenericParam,
    Generics, ImplItem, ImplItemFn, Item, ItemFn, ItemForeignMod, ItemImpl, ItemMacro, ItemMod,
    ItemTrait, Lit, Macro, Meta, Pat, PatIdent, Safety, Signature, Stmt, Token, TraitItem,
    TraitItemFn, Type, UseTree, Variant,
    ext::IdentExt,
    parse::ParseStream,
    punctuated::Punctuated,
    visit::{self, Visit},
    visit_mut::{self, VisitMut},
};
use toml_edit::DocumentMut;

use super::{
    cfg::{Cfg, Truth},
    disallowed::{Disallowed, canonical},
};

/// The library to scan.
pub struct Crate<'a> {
    /// The crate name its code calls itself by, after `crate::`.
    pub name: &'a str,
    /// The package directory, which reported file names are relative to.
    pub root: &'a Path,
    /// The library's root source file.
    pub src: &'a Path,
    /// Whether the crate uses the 2015 edition, where `use` paths start at
    /// the crate root.
    pub edition_2015: bool,
    /// The crate's dependencies by the name its code uses, mapped to their
    /// library names.
    pub externs: &'a BTreeMap<String, String>,
    pub cfg: Cfg<'a>,
}

/// The kinds of `unsafe` code, counted where they are written.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UnsafeCounts {
    /// `unsafe fn`, free or in a trait or impl.
    pub functions: usize,
    /// `unsafe { ... }` blocks.
    pub blocks: usize,
    /// `unsafe impl`.
    pub impls: usize,
    /// `unsafe trait`.
    pub traits: usize,
    /// `unsafe extern` blocks. A plain `extern` block, which editions before
    /// 2024 allow, isn't counted: its functions can only be called in
    /// `unsafe` blocks, which are.
    pub extern_blocks: usize,
    /// Attributes that `unsafe_code` reports, such as `#[no_mangle]`, with
    /// or without the `unsafe(...)` the 2024 edition requires, including
    /// those a `cfg_attr` adds.
    pub attributes: usize,
    /// `unsafe` in macro bodies that syn can't parse as code, such as
    /// `macro_rules!` definitions.
    pub in_macros: usize,
}

impl UnsafeCounts {
    pub fn total(&self) -> usize {
        self.functions
            + self.blocks
            + self.impls
            + self.traits
            + self.extern_blocks
            + self.attributes
            + self.in_macros
    }
}

/// A place in a dependency's source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

/// Source the scan couldn't read, so its counts leave it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unscanned {
    pub file: String,
    pub line: Option<usize>,
    pub reason: String,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The library declares `#![forbid(unsafe_code)]`, or its manifest
    /// forbids `unsafe_code`.
    pub forbids_unsafe: bool,
    pub unsafe_code: UnsafeCounts,
    /// The sites of each disallowed path it uses, by entry index.
    pub calls: BTreeMap<usize, Vec<Site>>,
    /// Calls that might reach a disallowed path but couldn't be resolved,
    /// counted by name.
    pub unresolved: BTreeMap<String, usize>,
    pub unscanned: Vec<Unscanned>,
}

/// Scans a library's module tree.
pub fn scan(krate: &Crate, disallowed: &Disallowed) -> Scan {
    let mut scanner = Scanner {
        krate,
        disallowed,
        scan: Scan {
            forbids_unsafe: manifest_forbids_unsafe(krate.root),
            ..Scan::default()
        },
        visited: HashSet::new(),
        files: Vec::new(),
        modules: HashMap::new(),
        definitions: HashMap::new(),
    };
    // Every module's names are bound before any code is read, so a path can
    // be followed through another module's `use` items.
    scanner.load(krate.src, vec![krate.name.to_owned()], true, None);
    scanner.definitions = scanner.definitions();
    // Visiting can load more files: modules declared inside functions.
    let mut visited = Vec::new();
    while !scanner.files.is_empty() {
        let files = mem::take(&mut scanner.files);
        for loaded in &files {
            let mut visitor = FileVisitor {
                scanner: &mut scanner,
                file: loaded.rel.clone(),
                module: loaded.module.clone(),
                mod_dir: loaded.mod_dir.clone(),
                path_dir: loaded.path_dir.clone(),
                blocks: Vec::new(),
                attributes: HashSet::new(),
            };
            for attr in &loaded.file.attrs {
                visitor.visit_attribute(attr);
            }
            for item in &loaded.file.items {
                visitor.visit_item(item);
            }
        }
        visited.extend(files);
    }
    drop(visited);
    // Spans keep every parsed file's text; nothing refers to them now.
    proc_macro2::extra::invalidate_current_thread_spans();
    scanner.scan
}

/// Whether a package's `Cargo.toml` sets `unsafe_code = "forbid"` in its
/// `[lints.rust]` table.
fn manifest_forbids_unsafe(root: &Path) -> bool {
    let Ok(text) = fs::read_to_string(root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return false;
    };
    let Some(level) = doc
        .get("lints")
        .and_then(|lints| lints.get("rust"))
        .and_then(|rust| rust.get("unsafe_code"))
    else {
        return false;
    };
    level
        .as_str()
        .or_else(|| level.get("level").and_then(|level| level.as_str()))
        == Some("forbid")
}

/// What a name is bound to in a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// An item with a known path.
    Item(Vec<String>),
    /// A `use` path, resolved in the module that has it.
    Use { path: Vec<String>, leading: bool },
    /// An item local to a block, which has no path.
    Opaque,
    /// Bindings of one name that any build may have, such as two `use`
    /// items behind `cfg`s the audit can't know: a path through the name may
    /// mean any of them.
    Either(Vec<Binding>),
}

impl Binding {
    fn alternatives(&self) -> &[Self] {
        match self {
            Self::Either(alternatives) => alternatives,
            other => slice::from_ref(other),
        }
    }
}

/// The namespaces a name can be bound in. Modules and types share one,
/// and functions, constants and statics the other, so `mod spawn;` and
/// `pub use self::spawn::spawn;` bind different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Types,
    Values,
}

/// A name's binding in one namespace of a scope.
#[derive(Debug)]
struct Bound {
    binding: Binding,
    /// Whether every build has it: the `cfg`s on it are known to hold.
    certain: bool,
}

impl Bound {
    /// Adds another binding the name may have.
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
        self.binding = match <[Binding; 1]>::try_from(alternatives) {
            Ok([only]) => only,
            Err(alternatives) => Binding::Either(alternatives),
        };
    }
}

/// The names one module or block binds.
#[derive(Debug, Default)]
struct Scope {
    types: HashMap<String, Bound>,
    values: HashMap<String, Bound>,
    globs: bool,
}

impl Scope {
    fn map(&mut self, namespace: Namespace) -> &mut HashMap<String, Bound> {
        match namespace {
            Namespace::Types => &mut self.types,
            Namespace::Values => &mut self.values,
        }
    }

    fn get(&self, namespace: Namespace, name: &str) -> Option<&Binding> {
        let map = match namespace {
            Namespace::Types => &self.types,
            Namespace::Values => &self.values,
        };
        map.get(name).map(|bound| &bound.binding)
    }

    /// Binds a local variable or a generic parameter, which hides what the
    /// scope bound to its name before.
    fn shadow(&mut self, namespace: Namespace, name: String) {
        self.map(namespace).insert(
            name,
            Bound {
                binding: Binding::Opaque,
                certain: true,
            },
        );
    }

    /// Binds an item defined here. `certain` is whether every build has
    /// it. Two items of one name can't both be in a build, so where one of
    /// them depends on a `cfg` the audit can't know, either may be.
    fn define(&mut self, namespace: Namespace, name: String, binding: Binding, certain: bool) {
        let map = self.map(namespace);
        match map.get_mut(&name) {
            Some(bound) if !(certain && bound.certain) => bound.add(binding),
            _ => {
                map.insert(name, Bound { binding, certain });
            }
        }
    }

    /// Binds a `use` in both namespaces, since the audit can't tell which
    /// its target is in. Where every build defines an item of the same name,
    /// the `use` must be in the other namespace. Two `use` items of one name
    /// that every build has are in different namespaces, so either may be
    /// the one a path means.
    fn import(&mut self, name: String, binding: Binding, certain: bool) {
        for map in [&mut self.types, &mut self.values] {
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
                        && matches!(bound.binding, Binding::Item(_) | Binding::Opaque) => {}
                Some(bound) => bound.add(binding.clone()),
            }
        }
    }

    /// Adds the names another scope of the same module binds, as where two
    /// modules of one path are each behind a `cfg`: either may be built.
    fn merge(&mut self, other: Self) {
        for (namespace, map) in [
            (Namespace::Types, other.types),
            (Namespace::Values, other.values),
        ] {
            for (name, bound) in map {
                self.define(namespace, name, bound.binding, false);
            }
        }
        self.globs |= other.globs;
    }
}

/// The namespace of a path's segment: the last one is in `last`, and the
/// ones before it name modules or types.
fn namespace(path: &[String], index: usize, last: Namespace) -> Namespace {
    if index + 1 == path.len() {
        last
    } else {
        Namespace::Types
    }
}

/// What a path resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Path(Vec<String>),
    /// A local variable or an item local to a block.
    Local,
    Unknown,
    /// What a path through a name with more than one binding may mean.
    Either(Vec<Resolved>),
}

impl Resolved {
    /// What a path may mean when it may mean any of `alternatives`.
    fn either(alternatives: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for alternative in alternatives {
            for resolved in alternative.into_alternatives() {
                if !flat.contains(&resolved) {
                    flat.push(resolved);
                }
            }
        }
        match <[Self; 1]>::try_from(flat) {
            Ok([only]) => only,
            Err(flat) if flat.is_empty() => Self::Unknown,
            Err(flat) => Self::Either(flat),
        }
    }

    fn alternatives(&self) -> &[Self] {
        match self {
            Self::Either(alternatives) => alternatives,
            other => slice::from_ref(other),
        }
    }

    fn into_alternatives(self) -> Vec<Self> {
        match self {
            Self::Either(alternatives) => alternatives,
            other => vec![other],
        }
    }

    /// Applies `f` to each path the resolution may be.
    fn map_paths(self, mut f: impl FnMut(Vec<String>) -> Self) -> Self {
        Self::either(
            self.into_alternatives()
                .into_iter()
                .map(|resolved| match resolved {
                    Self::Path(path) => f(path),
                    other => other,
                }),
        )
    }

    fn join(self, rest: &[String]) -> Self {
        self.map_paths(|mut path| {
            path.extend(rest.iter().cloned());
            Self::Path(path)
        })
    }
}

/// How many `use` items resolution follows before it gives up.
const MAX_DEPTH: usize = 16;

/// The standard prelude's names that a path can start with.
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

/// The crates every crate can name without declaring them.
const SYSROOT: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

/// A parsed file of the module tree.
struct Loaded {
    file: File,
    /// Its name relative to the package.
    rel: String,
    module: Vec<String>,
    /// Where its child modules' files are.
    mod_dir: PathBuf,
    /// What its `#[path]` attributes are relative to.
    path_dir: PathBuf,
}

/// Where a module's code is.
enum Location {
    Inline { dir: PathBuf },
    File { path: PathBuf, mod_rs: bool },
    Unscannable(String),
}

struct Scanner<'a> {
    krate: &'a Crate<'a>,
    disallowed: &'a Disallowed,
    scan: Scan,
    visited: HashSet<PathBuf>,
    files: Vec<Loaded>,
    /// The names each module binds, by module path.
    modules: HashMap<Vec<String>, Scope>,
    /// Where the disallowed paths into this crate are defined.
    definitions: HashMap<Vec<String>, usize>,
}

impl Scanner<'_> {
    /// Loads the file of the module at `module`, and the modules it declares.
    /// A mod-rs file, such as the crate root, a `mod.rs` or a file named by
    /// `#[path]`, keeps its child modules' files beside it; any other
    /// `name.rs` keeps them in `name/`. `from` is the declaration that names
    /// the file.
    fn load(&mut self, path: &Path, module: Vec<String>, mod_rs: bool, from: Option<(&str, Span)>) {
        let rel = self.relative(path);
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let reason = format!("reading {rel}: {error}");
                self.scan.unscanned.push(match from {
                    Some((file, span)) => Unscanned {
                        file: file.to_owned(),
                        line: Some(span.start().line),
                        reason,
                    },
                    None => Unscanned {
                        file: rel,
                        line: None,
                        reason,
                    },
                });
                return;
            }
        };
        if !self.visited.insert(path.to_path_buf()) {
            return;
        }
        let mut file = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(error) => {
                self.scan.unscanned.push(Unscanned {
                    file: rel,
                    line: Some(error.span().start().line),
                    reason: format!("syn could not parse it: {error}"),
                });
                return;
            }
        };
        if self.krate.cfg.enabled(&file.attrs) == Truth::False {
            return;
        }
        Expand.visit_file_mut(&mut file);
        if module.len() == 1 {
            self.scan.forbids_unsafe |= self.forbids_unsafe(&file.attrs);
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let mod_dir = if mod_rs {
            dir.clone()
        } else {
            dir.join(path.file_stem().unwrap_or_default())
        };
        self.index(&file.items, &module, &mod_dir, &dir, &rel);
        self.files.push(Loaded {
            file,
            rel,
            module,
            mod_dir,
            path_dir: dir,
        });
    }

    /// Binds the names of the module at `module`, and loads its child
    /// modules. Their `mod name;` files are in `mod_dir`, and their `#[path]`
    /// attributes are relative to `path_dir`.
    fn index(
        &mut self,
        items: &[Item],
        module: &[String],
        mod_dir: &Path,
        path_dir: &Path,
        rel: &str,
    ) {
        let scope = self.bind(items, Some(module));
        match self.modules.get_mut(module) {
            Some(existing) => existing.merge(scope),
            None => {
                self.modules.insert(module.to_vec(), scope);
            }
        }
        for item in items {
            let Item::Mod(item) = item else {
                continue;
            };
            if self.krate.cfg.enabled(&item.attrs) == Truth::False {
                continue;
            }
            let mut child = module.to_vec();
            child.push(item.ident.unraw().to_string());
            match self.locate(item, mod_dir, path_dir) {
                Location::Inline { dir } => {
                    if let Some((_, items)) = &item.content {
                        self.index(items, &child, &dir, &dir, rel);
                    }
                }
                Location::File { path, mod_rs } => {
                    self.load(&path, child, mod_rs, Some((rel, item.ident.span())));
                }
                Location::Unscannable(reason) => self.scan.unscanned.push(Unscanned {
                    file: rel.to_owned(),
                    line: Some(item.ident.span().start().line),
                    reason,
                }),
            }
        }
    }

    fn locate(&self, item: &ItemMod, mod_dir: &Path, path_dir: &Path) -> Location {
        let name = item.ident.unraw().to_string();
        let mut path = None;
        for (applies, meta) in self.krate.cfg.effective(&item.attrs) {
            let Meta::NameValue(value) = &meta else {
                continue;
            };
            let Expr::Lit(ExprLit {
                lit: Lit::Str(lit), ..
            }) = &value.value
            else {
                continue;
            };
            if !value.path.is_ident("path") {
                continue;
            }
            if applies == Truth::Unknown {
                return Location::Unscannable(format!(
                    "module `{name}` has a #[path] that depends on an unknown cfg"
                ));
            }
            path = Some(lit.value());
        }
        if item.content.is_some() {
            return Location::Inline {
                dir: match path {
                    Some(path) => path_dir.join(path),
                    None => mod_dir.join(&name),
                },
            };
        }
        if let Some(path) = path {
            return Location::File {
                path: path_dir.join(path),
                mod_rs: true,
            };
        }
        [
            (mod_dir.join(format!("{name}.rs")), false),
            (mod_dir.join(&name).join("mod.rs"), true),
        ]
        .into_iter()
        .find(|(path, _)| path.is_file())
        .map_or_else(
            || {
                Location::Unscannable(format!(
                    "module `{name}` has no file: neither {name}.rs nor {name}/mod.rs"
                ))
            },
            |(path, mod_rs)| Location::File { path, mod_rs },
        )
    }

    /// A file's name relative to the package, with `/` between components,
    /// or its full path if it is outside the package. A `#[path]` with `..`
    /// is resolved first, so the name is the file the compiler reads.
    fn relative(&self, path: &Path) -> String {
        let mut normal = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir
                    if matches!(normal.components().next_back(), Some(Component::Normal(_))) =>
                {
                    normal.pop();
                }
                component => normal.push(component),
            }
        }
        let path = normal.strip_prefix(self.krate.root).unwrap_or(&normal);
        path.components()
            .map(|component| match component {
                Component::RootDir => String::new(),
                component => component.as_os_str().to_string_lossy().into_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn forbids_unsafe(&self, attrs: &[Attribute]) -> bool {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .any(|(applies, meta)| {
                *applies == Truth::True
                    && meta.path().is_ident("forbid")
                    && matches!(meta, Meta::List(list) if list
                    .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                    .is_ok_and(|lints| lints.iter().any(|lint| lint.is_ident("unsafe_code"))))
            })
    }

    /// The names that `items` bind. Items of a module have paths under
    /// `module`; items of a block have none.
    fn bind<'i>(
        &self,
        items: impl IntoIterator<Item = &'i Item>,
        module: Option<&[String]>,
    ) -> Scope {
        let mut scope = Scope::default();
        for item in items {
            let certain = match self.krate.cfg.enabled(item_attrs(item)) {
                Truth::True => true,
                Truth::Unknown => false,
                Truth::False => continue,
            };
            let ident = match item {
                Item::Use(item) => {
                    let leading = item.leading_colon.is_some();
                    bind_use(&item.tree, &mut Vec::new(), leading, certain, &mut scope);
                    continue;
                }
                Item::ExternCrate(item) => {
                    let name = item
                        .rename
                        .as_ref()
                        .map_or(&item.ident, |(_, rename)| rename);
                    let target = item.ident.unraw().to_string();
                    let target = if target == "self" {
                        self.krate.name.to_owned()
                    } else {
                        self.extern_crate(&target).unwrap_or(target)
                    };
                    scope.define(
                        Namespace::Types,
                        name.unraw().to_string(),
                        Binding::Item(vec![target]),
                        certain,
                    );
                    continue;
                }
                Item::Const(item) => (&item.ident, &[Namespace::Values][..]),
                Item::Enum(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Fn(item) => (&item.sig.ident, &[Namespace::Values][..]),
                Item::Mod(item) => (&item.ident, &[Namespace::Types][..]),
                Item::Static(item) => (&item.ident, &[Namespace::Values][..]),
                // A tuple or unit struct is a value too.
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
                    Binding::Item(path)
                }
                None => Binding::Opaque,
            };
            for namespace in namespaces {
                scope.define(*namespace, name.clone(), binding.clone(), certain);
            }
        }
        scope
    }

    /// The library name of a crate this crate can name without a `use`.
    fn extern_crate(&self, name: &str) -> Option<String> {
        if let Some(lib) = self.krate.externs.get(name) {
            return Some(lib.clone());
        }
        SYSROOT.contains(&name).then(|| name.to_owned())
    }

    /// The path that a leading `crate`, `self` or `super` leads to, with the
    /// rest of `path` appended, or `None` if `path` starts elsewhere.
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

    /// The binding of `name` in `namespace`, in the blocks around a place in
    /// `module` or in the module itself, with the blocks around the scope
    /// that binds it: a `use` resolves there, not where its name is used.
    fn lookup<'s>(
        &'s self,
        module: &[String],
        blocks: &'s [Scope],
        namespace: Namespace,
        name: &str,
    ) -> Option<(&'s Binding, &'s [Scope])> {
        for (index, scope) in blocks.iter().enumerate().rev() {
            if let Some(binding) = scope.get(namespace, name) {
                return Some((binding, blocks.get(..=index).unwrap_or_default()));
            }
        }
        self.modules
            .get(module)
            .and_then(|scope| scope.get(namespace, name))
            .map(|binding| (binding, &[][..]))
    }

    /// Resolves a binding in `namespace`, found in `module`.
    fn binding(
        &self,
        module: &[String],
        blocks: &[Scope],
        namespace: Namespace,
        binding: &Binding,
        depth: usize,
    ) -> Resolved {
        match binding {
            Binding::Item(path) => Resolved::Path(path.clone()),
            Binding::Opaque => Resolved::Local,
            Binding::Use { path, leading } => {
                self.use_path(module, blocks, namespace, path, *leading, depth + 1)
            }
            Binding::Either(alternatives) => Resolved::either(
                alternatives
                    .iter()
                    .map(|alternative| self.binding(module, blocks, namespace, alternative, depth)),
            ),
        }
    }

    /// Resolves a `use` path written in `module`, for its target in
    /// `namespace`.
    fn use_path(
        &self,
        module: &[String],
        blocks: &[Scope],
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
        if let Some(path) = self.relative_path(module, path) {
            return self.follow(path, namespace, depth);
        }
        if self.krate.edition_2015 {
            return self.root_path(path, namespace, depth);
        }
        let unbound = || match self.extern_crate(first) {
            Some(krate) => Resolved::Path(vec![krate]).join(rest),
            None => Resolved::Unknown,
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if !leading
            && let Some((binding, scope)) = self.lookup(module, blocks, first_namespace, first)
        {
            return self.use_binding(module, scope, binding, path, namespace, depth, unbound);
        }
        unbound()
    }

    /// Resolves a `use` path written in `module`, whose first segment is
    /// bound to `binding` in `scope`. A `use` can't refer to itself: in
    /// `use tokio;`, `tokio` is the crate. So where the binding is the `use`
    /// of the path, the path resolves as `unbound` says a path whose first
    /// segment isn't bound does.
    #[expect(
        clippy::too_many_arguments,
        reason = "each is a part of the place being resolved"
    )]
    fn use_binding(
        &self,
        module: &[String],
        scope: &[Scope],
        binding: &Binding,
        path: &[String],
        namespace: Namespace,
        depth: usize,
        unbound: impl FnOnce() -> Resolved,
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
                .binding(module, scope, first_namespace, alternative, depth)
                .join(rest);
            alternatives.push(self.followed(resolved, namespace, depth));
        }
        if itself {
            alternatives.push(unbound());
        }
        Resolved::either(alternatives)
    }

    /// Resolves a 2015 `use` path, which starts at the crate root.
    fn root_path(&self, path: &[String], namespace: Namespace, depth: usize) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        let root = [self.krate.name.to_owned()];
        let unbound = || {
            if let Some(krate) = self.extern_crate(first) {
                return Resolved::Path(vec![krate]).join(rest);
            }
            let mut full = root.to_vec();
            full.extend(path.iter().cloned());
            self.follow(full, namespace, depth)
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some((binding, _)) = self.lookup(&root, &[], first_namespace, first) {
            return self.use_binding(&root, &[], binding, path, namespace, depth, unbound);
        }
        unbound()
    }

    fn followed(&self, resolved: Resolved, namespace: Namespace, depth: usize) -> Resolved {
        resolved.map_paths(|path| self.follow(path, namespace, depth))
    }

    /// Follows a path into this crate through the `use` items of the modules
    /// it passes, as `crate::sync::Mutex` reaches `std::sync::Mutex` through
    /// `mod sync { pub use std::sync::Mutex; }`. It stops at a disallowed
    /// path, which names a re-export as often as a definition.
    fn follow(&self, path: Vec<String>, namespace: Namespace, depth: usize) -> Resolved {
        if self.disallowed.entry(&path).is_some() {
            return Resolved::Path(path);
        }
        self.step(path, namespace, depth)
    }

    fn step(&self, path: Vec<String>, namespace: Namespace, depth: usize) -> Resolved {
        self.walk(path, 1, namespace, depth)
    }

    /// Follows a path into this crate from its segment at `start`.
    fn walk(
        &self,
        path: Vec<String>,
        start: usize,
        namespace: Namespace,
        depth: usize,
    ) -> Resolved {
        if path.first().map(String::as_str) != Some(self.krate.name) || depth > MAX_DEPTH {
            return Resolved::Path(path);
        }
        for end in start..path.len() {
            let (Some(module), Some(name)) = (path.get(..end), path.get(end)) else {
                break;
            };
            let Some(scope) = self.modules.get(module) else {
                // A type or a function, whose items are its own.
                break;
            };
            let name_namespace = self::namespace(&path, end, namespace);
            match scope.get(name_namespace, name) {
                Some(binding @ (Binding::Use { .. } | Binding::Either(_))) => {
                    let rest = path.get(end + 1..).unwrap_or_default();
                    return Resolved::either(binding.alternatives().iter().map(|alternative| {
                        if let Binding::Use { .. } = alternative {
                            let resolved = self
                                .binding(module, &[], name_namespace, alternative, depth + 1)
                                .join(rest);
                            self.followed(resolved, namespace, depth + 1)
                        } else {
                            self.walk(path.clone(), end + 1, namespace, depth + 1)
                        }
                    }));
                }
                Some(_) => {}
                None if scope.globs => return Resolved::Unknown,
                None => break,
            }
        }
        Resolved::Path(path)
    }

    /// Where the disallowed paths into this crate are defined, mapped to
    /// their entries, so that a path reaching the definition another way
    /// matches too, as Clippy's resolved paths do.
    fn definitions(&self) -> HashMap<Vec<String>, usize> {
        let mut definitions = HashMap::new();
        for (path, entry) in self.disallowed.paths() {
            if path.first().map(String::as_str) != Some(self.krate.name) {
                continue;
            }
            for definition in self
                .step(path.to_vec(), Namespace::Values, 0)
                .into_alternatives()
            {
                if let Resolved::Path(definition) = definition
                    && definition != path
                {
                    definitions.insert(definition, entry);
                }
            }
        }
        definitions
    }

    /// The entry a resolved path names.
    fn entry(&self, path: &[String]) -> Option<usize> {
        self.disallowed
            .entry(path)
            .or_else(|| self.definitions.get(path).copied())
    }

    /// Resolves a path written in `module` within `blocks`, for its last
    /// segment in `namespace`: values for a path in an expression, types
    /// for the type of a qualified path.
    fn path(
        &self,
        module: &[String],
        blocks: &[Scope],
        namespace: Namespace,
        path: &[String],
        leading: bool,
    ) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        // On its own, `self` is a method's receiver, not the module.
        if first == "self" && rest.is_empty() {
            return Resolved::Local;
        }
        if let Some(path) = self.relative_path(module, path) {
            return self.follow(path, namespace, 0);
        }
        if leading {
            if self.krate.edition_2015 {
                return self.root_path(path, namespace, 0);
            }
            return match self.extern_crate(first) {
                Some(krate) => Resolved::Path(vec![krate]).join(rest),
                None => Resolved::Unknown,
            };
        }
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some((binding, scope)) = self.lookup(module, blocks, first_namespace, first) {
            let resolved = self
                .binding(module, scope, first_namespace, binding, 0)
                .join(rest);
            return self.followed(resolved, namespace, 0);
        }
        if let Some(krate) = self.extern_crate(first) {
            return Resolved::Path(vec![krate]).join(rest);
        }
        // A glob import shadows the prelude and may provide any name. It
        // could shadow a crate name too, but no crate exports an item named
        // like another crate, while a `Vec` or `Box` of its own is common.
        let globs = blocks
            .iter()
            .chain(self.modules.get(module))
            .any(|scope| scope.globs);
        if !globs && let Some((_, prelude)) = PRELUDE.iter().find(|(name, _)| name == first) {
            return Resolved::Path(prelude.split("::").map(str::to_owned).collect()).join(rest);
        }
        Resolved::Unknown
    }
}

/// Whether `binding` is the `use` of `path`, which the path itself can't
/// refer to: in `use tokio;`, `tokio` is the crate.
fn is_use_of(binding: &Binding, path: &[String]) -> bool {
    matches!(binding, Binding::Use { path: used, .. } if used == path)
}

/// Records the names a `use` tree binds, under the path `prefix` leads to.
/// `certain` is whether every build has the `use`.
fn bind_use(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    leading: bool,
    certain: bool,
    scope: &mut Scope,
) {
    let mut bind = |name: String, path: Vec<String>| {
        if name != "_" {
            scope.import(name, Binding::Use { path, leading }, certain);
        }
    };
    match tree {
        UseTree::Path(tree) => {
            prefix.push(tree.ident.unraw().to_string());
            bind_use(&tree.tree, prefix, leading, certain, scope);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let ident = name.ident.unraw().to_string();
            if ident == "self" {
                if let Some(last) = prefix.last() {
                    bind(last.clone(), prefix.clone());
                }
            } else {
                let mut path = prefix.clone();
                path.push(ident.clone());
                bind(ident, path);
            }
        }
        UseTree::Rename(rename) => {
            let ident = rename.ident.unraw().to_string();
            let mut path = prefix.clone();
            if ident != "self" {
                path.push(ident);
            }
            bind(rename.rename.unraw().to_string(), path);
        }
        UseTree::Glob(_) => scope.globs = true,
        UseTree::Group(group) => {
            for tree in &group.items {
                bind_use(tree, prefix, leading, certain, scope);
            }
        }
    }
}

/// Replaces each item macro whose body parses as items with those items, as
/// macros such as Tokio's `cfg_rt! { ... }` expand to their input. The
/// macro's own attributes carry over to each item, so a `cfg` on it still
/// applies.
struct Expand;

impl Expand {
    fn items(&mut self, items: &mut Vec<Item>) {
        let mut expanded = Vec::with_capacity(items.len());
        for item in mem::take(items) {
            let Item::Macro(item) = item else {
                expanded.push(item);
                continue;
            };
            if item.ident.is_some() || is_include(&item.mac) {
                expanded.push(Item::Macro(item));
                continue;
            }
            match item.mac.parse_body_with(parse_items) {
                Ok(mut inner) => {
                    self.items(&mut inner);
                    for mut inner in inner {
                        if let Some(attrs) = item_attrs_mut(&mut inner) {
                            let own = mem::take(attrs);
                            attrs.extend(item.attrs.iter().cloned());
                            attrs.extend(own);
                        }
                        expanded.push(inner);
                    }
                }
                Err(_) => expanded.push(Item::Macro(item)),
            }
        }
        *items = expanded;
    }
}

impl VisitMut for Expand {
    fn visit_file_mut(&mut self, file: &mut File) {
        self.items(&mut file.items);
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, item: &mut ItemMod) {
        if let Some((_, items)) = &mut item.content {
            self.items(items);
        }
        visit_mut::visit_item_mod_mut(self, item);
    }
}

fn parse_items(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut items = Vec::new();
    while !input.is_empty() {
        items.push(input.parse()?);
    }
    Ok(items)
}

fn is_include(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "include")
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn item_attrs_mut(item: &mut Item) -> Option<&mut Vec<Attribute>> {
    Some(match item {
        Item::Const(item) => &mut item.attrs,
        Item::Enum(item) => &mut item.attrs,
        Item::ExternCrate(item) => &mut item.attrs,
        Item::Fn(item) => &mut item.attrs,
        Item::ForeignMod(item) => &mut item.attrs,
        Item::Impl(item) => &mut item.attrs,
        Item::Macro(item) => &mut item.attrs,
        Item::Mod(item) => &mut item.attrs,
        Item::Static(item) => &mut item.attrs,
        Item::Struct(item) => &mut item.attrs,
        Item::Trait(item) => &mut item.attrs,
        Item::TraitAlias(item) => &mut item.attrs,
        Item::Type(item) => &mut item.attrs,
        Item::Union(item) => &mut item.attrs,
        Item::Use(item) => &mut item.attrs,
        _ => return None,
    })
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(expr) => &expr.attrs,
        Expr::Assign(expr) => &expr.attrs,
        Expr::Async(expr) => &expr.attrs,
        Expr::Await(expr) => &expr.attrs,
        Expr::Binary(expr) => &expr.attrs,
        Expr::Block(expr) => &expr.attrs,
        Expr::Break(expr) => &expr.attrs,
        Expr::Call(expr) => &expr.attrs,
        Expr::Cast(expr) => &expr.attrs,
        Expr::Closure(expr) => &expr.attrs,
        Expr::Const(expr) => &expr.attrs,
        Expr::Continue(expr) => &expr.attrs,
        Expr::Field(expr) => &expr.attrs,
        Expr::ForLoop(expr) => &expr.attrs,
        Expr::Group(expr) => &expr.attrs,
        Expr::If(expr) => &expr.attrs,
        Expr::Index(expr) => &expr.attrs,
        Expr::Infer(expr) => &expr.attrs,
        Expr::Let(expr) => &expr.attrs,
        Expr::Lit(expr) => &expr.attrs,
        Expr::Loop(expr) => &expr.attrs,
        Expr::Macro(expr) => &expr.attrs,
        Expr::Match(expr) => &expr.attrs,
        Expr::MethodCall(expr) => &expr.attrs,
        Expr::Paren(expr) => &expr.attrs,
        Expr::Path(expr) => &expr.attrs,
        Expr::Range(expr) => &expr.attrs,
        Expr::RawAddr(expr) => &expr.attrs,
        Expr::Reference(expr) => &expr.attrs,
        Expr::Repeat(expr) => &expr.attrs,
        Expr::Return(expr) => &expr.attrs,
        Expr::Struct(expr) => &expr.attrs,
        Expr::Try(expr) => &expr.attrs,
        Expr::TryBlock(expr) => &expr.attrs,
        Expr::Tuple(expr) => &expr.attrs,
        Expr::Unary(expr) => &expr.attrs,
        Expr::Unsafe(expr) => &expr.attrs,
        Expr::While(expr) => &expr.attrs,
        Expr::Yield(expr) => &expr.attrs,
        _ => &[],
    }
}

fn is_unsafe(safety: &Safety) -> bool {
    matches!(safety, Safety::Unsafe(_))
}

/// The attributes that the `unsafe_code` lint reports when they are written
/// without `unsafe(...)`, as editions before 2024 allow: rustc's unsafe
/// attributes, stable and unstable, and `allow_internal_unsafe`.
const UNSAFE_ATTRIBUTES: [&str; 8] = [
    "no_mangle",
    "export_name",
    "link_section",
    "naked",
    "ffi_pure",
    "ffi_const",
    "force_target_feature",
    "allow_internal_unsafe",
];

/// Whether `unsafe_code` reports an attribute. rustc accepts `unsafe(...)`
/// only around an unsafe attribute, so any attribute written that way is
/// one.
fn is_unsafe_attribute(meta: &Meta) -> bool {
    let path = meta.path();
    path.is_ident("unsafe") || UNSAFE_ATTRIBUTES.iter().any(|name| path.is_ident(name))
}

/// Walks one module's syntax tree.
struct FileVisitor<'s, 'a> {
    scanner: &'s mut Scanner<'a>,
    /// The file's name relative to the package.
    file: String,
    module: Vec<String>,
    /// Where the current module's `mod name;` files are.
    mod_dir: PathBuf,
    /// What the current module's `#[path]` attributes are relative to.
    path_dir: PathBuf,
    /// The names bound by the blocks around the current place.
    blocks: Vec<Scope>,
    /// Where the attributes already visited are written, by line and
    /// column.
    attributes: HashSet<(usize, usize)>,
}

impl FileVisitor<'_, '_> {
    fn enabled(&self, attrs: &[Attribute]) -> bool {
        self.scanner.krate.cfg.enabled(attrs) != Truth::False
    }

    fn site(&self, span: Span) -> Site {
        let start = span.start();
        Site {
            file: self.file.clone(),
            line: start.line,
            column: start.column + 1,
        }
    }

    fn unscanned(&mut self, span: Span, reason: String) {
        let site = self.site(span);
        self.scanner.scan.unscanned.push(Unscanned {
            file: site.file,
            line: Some(site.line),
            reason,
        });
    }

    /// The attributes that `unsafe_code` reports among those one attribute,
    /// written as `meta`, stands for once `cfg_attr` is expanded.
    fn unsafe_attributes(&self, meta: Meta) -> Vec<Meta> {
        self.scanner
            .krate
            .cfg
            .expanded(meta)
            .into_iter()
            .map(|(_, meta)| meta)
            .filter(is_unsafe_attribute)
            .collect()
    }

    fn unresolved(&mut self, name: &str) {
        *self
            .scanner
            .scan
            .unresolved
            .entry(name.to_owned())
            .or_default() += 1;
    }

    fn resolve(&self, expr: &ExprPath) -> Resolved {
        let scanner = &*self.scanner;
        let names = segments(&expr.path);
        let leading = expr.path.leading_colon.is_some();
        let resolved = match &expr.qself {
            // `<[T]>::split_at` and `<Vec<T>>::insert` name the type's items.
            Some(qself) if qself.position == 0 => {
                let base = match &*qself.ty {
                    Type::Slice(_) => Resolved::Path(vec!["slice".to_owned()]),
                    Type::Path(ty) if ty.qself.is_none() => scanner.path(
                        &self.module,
                        &self.blocks,
                        Namespace::Types,
                        &segments(&ty.path),
                        ty.path.leading_colon.is_some(),
                    ),
                    _ => Resolved::Unknown,
                };
                base.join(&names)
            }
            // `<T as Trait>::method` names the trait's item.
            _ => scanner.path(
                &self.module,
                &self.blocks,
                Namespace::Values,
                &names,
                leading,
            ),
        };
        resolved.map_paths(|path| Resolved::Path(canonical(path)))
    }

    fn start(expr: &ExprPath) -> Span {
        if let Some(qself) = &expr.qself {
            return qself.lt_token.span;
        }
        if let Some(colon) = &expr.path.leading_colon {
            let [first, _] = colon.spans;
            return first;
        }
        expr.path
            .segments
            .first()
            .map_or_else(Span::call_site, |segment| segment.ident.span())
    }

    /// Counts what can be seen in tokens syn can't parse as code: `unsafe`,
    /// and calls to a name some disallowed path has.
    fn tokens(&mut self, stream: TokenStream) {
        let mut previous: Option<TokenTree> = None;
        let mut before: Option<TokenTree> = None;
        for tree in stream {
            match &tree {
                TokenTree::Ident(ident) if ident == "unsafe" => {
                    self.scanner.scan.unsafe_code.in_macros += 1;
                }
                TokenTree::Group(group) => {
                    if group.delimiter() == Delimiter::Parenthesis
                        && let Some(TokenTree::Ident(name)) = &previous
                    {
                        let name = name.unraw().to_string();
                        let disallowed = self.scanner.disallowed;
                        let counts = match &before {
                            Some(TokenTree::Ident(ident)) if ident == "fn" => false,
                            Some(TokenTree::Punct(punct)) if punct.as_char() == '.' => {
                                disallowed.has_method(&name)
                            }
                            _ => disallowed.has_name(&name),
                        };
                        if counts {
                            self.unresolved(&name);
                        }
                    }
                    self.tokens(group.stream());
                }
                _ => {}
            }
            before = previous.replace(tree);
        }
    }

    fn module_item(&mut self, item: &ItemMod) {
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        let mut module = self.module.clone();
        module.push(item.ident.unraw().to_string());
        let location = self.scanner.locate(item, &self.mod_dir, &self.path_dir);
        match (&item.content, location) {
            (Some((_, items)), location) => {
                let dir = match location {
                    Location::Inline { dir } => dir,
                    _ => self.mod_dir.join(item.ident.unraw().to_string()),
                };
                let outer = mem::replace(&mut self.module, module);
                let mod_dir = mem::replace(&mut self.mod_dir, dir.clone());
                let path_dir = mem::replace(&mut self.path_dir, dir);
                let blocks = mem::take(&mut self.blocks);
                for item in items {
                    self.visit_item(item);
                }
                self.module = outer;
                self.mod_dir = mod_dir;
                self.path_dir = path_dir;
                self.blocks = blocks;
            }
            // A module of a module was loaded with the module tree; one
            // declared inside a function is loaded now.
            (None, _) if self.blocks.is_empty() => {}
            (None, Location::File { path, mod_rs }) => {
                let from = (self.file.as_str(), item.ident.span());
                self.scanner.load(&path, module, mod_rs, Some(from));
            }
            (None, Location::Unscannable(reason)) => self.unscanned(item.ident.span(), reason),
            (None, Location::Inline { .. }) => {}
        }
    }

    /// Visits statements with the items among them in scope.
    fn stmts(&mut self, stmts: &[Stmt]) {
        let items = stmts.iter().filter_map(|stmt| match stmt {
            Stmt::Item(item) => Some(item),
            _ => None,
        });
        let scope = self.scanner.bind(items, None);
        self.blocks.push(scope);
        for stmt in stmts {
            self.visit_stmt(stmt);
            // A `let` binds its names for the statements after it.
            if let Stmt::Local(local) = stmt
                && self.enabled(&local.attrs)
                && let Some(block) = self.blocks.last_mut()
            {
                for name in pattern_names([&local.pat]) {
                    block.shadow(Namespace::Values, name);
                }
            }
        }
        self.blocks.pop();
    }

    /// Visits an `if` or `while` condition from left to right, binding each
    /// `let`'s names in the innermost block after its initializer, so that in
    /// `if let Some(x) = y && let Ok(z) = x` the second `let` sees `x`.
    fn condition(&mut self, condition: &Expr) {
        match condition {
            Expr::Let(expr) if self.enabled(&expr.attrs) => {
                self.visit_expr(&expr.expr);
                self.visit_pat(&expr.pat);
                if let Some(block) = self.blocks.last_mut() {
                    for name in pattern_names([&*expr.pat]) {
                        block.shadow(Namespace::Values, name);
                    }
                }
            }
            Expr::Binary(expr) if self.enabled(&expr.attrs) => {
                self.condition(&expr.left);
                self.condition(&expr.right);
            }
            Expr::Paren(expr) if self.enabled(&expr.attrs) => self.condition(&expr.expr),
            expr => self.visit_expr(expr),
        }
    }

    /// Visits code with an item's generic parameters in scope: a type
    /// parameter named `Vec` is not the prelude's.
    fn with_generics(&mut self, generics: &Generics, visit: impl FnOnce(&mut Self)) {
        let mut scope = Scope::default();
        for param in &generics.params {
            match param {
                GenericParam::Type(param) => {
                    scope.shadow(Namespace::Types, param.ident.unraw().to_string());
                }
                GenericParam::Const(param) => {
                    scope.shadow(Namespace::Values, param.ident.unraw().to_string());
                }
                _ => {}
            }
        }
        self.blocks.push(scope);
        visit(self);
        self.blocks.pop();
    }

    /// Visits code with the names some patterns bind in scope, such as a
    /// function's parameters or a match arm's pattern.
    fn with_patterns<'p>(
        &mut self,
        patterns: impl IntoIterator<Item = &'p Pat>,
        visit: impl FnOnce(&mut Self),
    ) {
        let mut scope = Scope::default();
        for name in pattern_names(patterns) {
            scope.shadow(Namespace::Values, name);
        }
        self.blocks.push(scope);
        visit(self);
        self.blocks.pop();
    }
}

/// The names that patterns bind.
fn pattern_names<'p>(patterns: impl IntoIterator<Item = &'p Pat>) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
            self.0.push(pat.ident.unraw().to_string());
            visit::visit_pat_ident(self, pat);
        }
    }
    let mut names = Names(Vec::new());
    for pattern in patterns {
        names.visit_pat(pattern);
    }
    names.0
}

/// The patterns of a signature's parameters.
fn parameters(sig: &Signature) -> impl Iterator<Item = &Pat> {
    sig.inputs.iter().filter_map(|input| match input {
        FnArg::Typed(typed) => Some(&*typed.pat),
        FnArg::Receiver(_) => None,
    })
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.unraw().to_string())
        .collect()
}

impl<'ast> Visit<'ast> for FileVisitor<'_, '_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !self.enabled(item_attrs(item)) {
            return;
        }
        match item {
            Item::Mod(item) => self.module_item(item),
            Item::Macro(item) => self.visit_item_macro(item),
            _ => visit::visit_item(self, item),
        }
    }

    fn visit_item_macro(&mut self, item: &'ast ItemMacro) {
        if item.ident.is_none() && is_include(&item.mac) {
            self.unscanned(
                item.mac.bang_token.span,
                "include! of another file is not followed".to_owned(),
            );
            return;
        }
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        // A `macro_rules!` definition, or an invocation whose body isn't
        // items.
        self.tokens(item.mac.tokens.clone());
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if is_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_item_fn(this, item);
            });
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if item.unsafety.is_some() {
            self.scanner.scan.unsafe_code.impls += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_impl(this, item));
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if item.unsafety.is_some() {
            self.scanner.scan.unsafe_code.traits += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_trait(this, item));
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast ItemForeignMod) {
        if item.unsafety.is_some() {
            self.scanner.scan.unsafe_code.extern_blocks += 1;
        }
        visit::visit_item_foreign_mod(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let attrs = match item {
            ImplItem::Const(item) => &item.attrs[..],
            ImplItem::Fn(item) => &item.attrs,
            ImplItem::Type(item) => &item.attrs,
            ImplItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if self.enabled(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if is_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_impl_item_fn(this, item);
            });
        });
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        let attrs = match item {
            TraitItem::Const(item) => &item.attrs[..],
            TraitItem::Fn(item) => &item.attrs,
            TraitItem::Type(item) => &item.attrs,
            TraitItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if self.enabled(attrs) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        if is_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_trait_item_fn(this, item);
            });
        });
    }

    /// `Expand` gives each item a macro invocation expands to a copy of the
    /// invocation's attributes, so an attribute counts only the first time
    /// it is seen where it is written.
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        let start = attr.pound_token.span.start();
        if self.attributes.insert((start.line, start.column)) {
            self.scanner.scan.unsafe_code.attributes +=
                self.unsafe_attributes(attr.meta.clone()).len();
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_block(&mut self, block: &'ast Block) {
        self.stmts(&block.stmts);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Items and expressions check their own attributes.
        let attrs = match stmt {
            Stmt::Local(local) => &local.attrs[..],
            Stmt::Macro(mac) => &mac.attrs,
            Stmt::Expr(..) | Stmt::Item(_) => &[],
        };
        if self.enabled(attrs) {
            visit::visit_stmt(self, stmt);
        }
    }

    /// Any expression can carry a `cfg`, such as an array element or a
    /// call's argument.
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.enabled(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }

    fn visit_field_value(&mut self, field: &'ast FieldValue) {
        if self.enabled(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    /// A variant's discriminant and a field's type can hold expressions,
    /// such as `X = unsafe { 0 }` or an array length.
    fn visit_variant(&mut self, variant: &'ast Variant) {
        if self.enabled(&variant.attrs) {
            visit::visit_variant(self, variant);
        }
    }

    fn visit_field(&mut self, field: &'ast Field) {
        if self.enabled(&field.attrs) {
            visit::visit_field(self, field);
        }
    }

    fn visit_arm(&mut self, arm: &'ast Arm) {
        if self.enabled(&arm.attrs) {
            self.with_patterns([&arm.pat], |this| visit::visit_arm(this, arm));
        }
    }

    fn visit_expr_closure(&mut self, expr: &'ast ExprClosure) {
        self.with_patterns(&expr.inputs, |this| visit::visit_expr_closure(this, expr));
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast ExprForLoop) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.visit_expr(&expr.expr);
        self.with_patterns([&*expr.pat], |this| this.visit_block(&expr.body));
    }

    fn visit_expr_if(&mut self, expr: &'ast ExprIf) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.then_branch);
        self.blocks.pop();
        if let Some((_, otherwise)) = &expr.else_branch {
            self.visit_expr(otherwise);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast ExprWhile) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.body);
        self.blocks.pop();
    }

    fn visit_expr_unsafe(&mut self, expr: &'ast ExprUnsafe) {
        self.scanner.scan.unsafe_code.blocks += 1;
        visit::visit_expr_unsafe(self, expr);
    }

    /// A path that may mean any of several items counts for each of them
    /// that is disallowed.
    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        let entries: BTreeSet<usize> = self
            .resolve(expr)
            .alternatives()
            .iter()
            .filter_map(|resolved| match resolved {
                Resolved::Path(path) => self.scanner.entry(path),
                _ => None,
            })
            .collect();
        for entry in entries {
            let site = self.site(Self::start(expr));
            self.scanner.scan.calls.entry(entry).or_default().push(site);
        }
        visit::visit_expr_path(self, expr);
    }

    fn visit_expr_call(&mut self, expr: &'ast ExprCall) {
        if let Expr::Path(func) = &*expr.func
            && let Some(name) = func
                .path
                .segments
                .last()
                .map(|s| s.ident.unraw().to_string())
            && self.scanner.disallowed.has_name(&name)
        {
            let disallowed = self.scanner.disallowed;
            let resolved = self.resolve(func);
            let unresolved = resolved
                .alternatives()
                .iter()
                .any(|resolved| match resolved {
                    // A path into a crate no entry names may be a re-export of
                    // a disallowed item, as `smol::Executor` is
                    // `async_executor::Executor`.
                    Resolved::Path(path) => {
                        self.scanner.entry(path).is_none()
                            && path
                                .first()
                                .is_none_or(|krate| !disallowed.names_crate(krate))
                    }
                    Resolved::Local => false,
                    // Alternatives are never nested, so `Either` isn't one.
                    Resolved::Unknown | Resolved::Either(_) => true,
                });
            if unresolved {
                self.unresolved(&name);
            }
        }
        visit::visit_expr_call(self, expr);
    }

    fn visit_expr_method_call(&mut self, expr: &'ast ExprMethodCall) {
        let name = expr.method.unraw().to_string();
        if self.scanner.disallowed.has_method(&name) {
            self.unresolved(&name);
        }
        visit::visit_expr_method_call(self, expr);
    }

    /// Macros in expressions and statements, whose bodies are often
    /// expressions, as in `println!` or `vec!`, or statements.
    fn visit_macro(&mut self, mac: &'ast Macro) {
        if is_include(mac) {
            self.unscanned(
                mac.bang_token.span,
                "include! of another file is not followed".to_owned(),
            );
            return;
        }
        if let Ok(exprs) = mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated) {
            for expr in &exprs {
                self.visit_expr(expr);
            }
        } else if let Ok(stmts) = mac.parse_body_with(Block::parse_within) {
            self.stmts(&stmts);
        } else {
            self.tokens(mac.tokens.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse::Parser;

    use super::*;

    /// Whether `unsafe_code` reports each attribute, as rustc 1.100 does.
    #[test]
    fn unsafe_attributes_are_the_ones_unsafe_code_reports() {
        for (source, reported) in [
            ("#[no_mangle]", true),
            ("#[export_name = \"name\"]", true),
            ("#[link_section = \".data\"]", true),
            ("#[naked]", true),
            ("#[ffi_pure]", true),
            ("#[ffi_const]", true),
            ("#[force_target_feature(enable = \"avx2\")]", true),
            ("#[allow_internal_unsafe]", true),
            ("#[unsafe(no_mangle)]", true),
            ("#[unsafe(export_name = \"name\")]", true),
            ("#[used]", false),
            ("#[link_name = \"name\"]", false),
            ("#[inline]", false),
            ("#[target_feature(enable = \"avx2\")]", false),
            ("#[allow(unsafe_code)]", false),
            ("#[other::no_mangle]", false),
        ] {
            let attrs = Attribute::parse_outer.parse_str(source).unwrap();
            let [attr] = &attrs[..] else {
                panic!("{source} is one attribute");
            };
            assert_eq!(is_unsafe_attribute(&attr.meta), reported, "{source}");
        }
    }
}
