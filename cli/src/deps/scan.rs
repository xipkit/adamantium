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
    collections::{BTreeMap, HashMap, HashSet},
    fs, mem,
    path::{Component, Path, PathBuf},
};

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use serde::Serialize;
use syn::{
    Arm, Attribute, Block, Expr, ExprCall, ExprClosure, ExprForLoop, ExprIf, ExprLit,
    ExprMethodCall, ExprPath, ExprUnsafe, ExprWhile, Field, FieldValue, File, FnArg, ImplItem,
    ImplItemFn, Item, ItemFn, ItemImpl, ItemMacro, ItemMod, ItemTrait, Lit, Macro, Meta, Pat,
    PatIdent, Safety, Signature, Stmt, Token, TraitItem, TraitItemFn, Type, UseTree, Variant,
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
    /// `unsafe` in macro bodies that syn can't parse as code, such as
    /// `macro_rules!` definitions.
    pub in_macros: usize,
}

impl UnsafeCounts {
    pub fn total(&self) -> usize {
        self.functions + self.blocks + self.impls + self.traits + self.in_macros
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
            };
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
#[derive(Debug, Clone)]
enum Binding {
    /// An item with a known path.
    Item(Vec<String>),
    /// A `use` path, resolved in the module that has it.
    Use { path: Vec<String>, leading: bool },
    /// An item local to a block, which has no path.
    Opaque,
}

/// The namespaces a name can be bound in. Modules and types share one,
/// and functions, constants and statics the other, so `mod spawn;` and
/// `pub use self::spawn::spawn;` bind different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Types,
    Values,
}

/// The names one module or block binds.
#[derive(Debug, Default)]
struct Scope {
    types: HashMap<String, Binding>,
    values: HashMap<String, Binding>,
    globs: bool,
}

impl Scope {
    fn get(&self, namespace: Namespace, name: &str) -> Option<&Binding> {
        match namespace {
            Namespace::Types => self.types.get(name),
            Namespace::Values => self.values.get(name),
        }
    }

    /// Binds an item defined here.
    fn define(&mut self, namespace: Namespace, name: String, binding: Binding) {
        match namespace {
            Namespace::Types => self.types.insert(name, binding),
            Namespace::Values => self.values.insert(name, binding),
        };
    }

    /// Binds a `use` in both namespaces, since the audit can't tell which
    /// its target is in. Where an item of the same name is defined, the
    /// `use` must be in the other namespace.
    fn import(&mut self, name: String, binding: Binding) {
        for map in [&mut self.types, &mut self.values] {
            if !matches!(map.get(&name), Some(Binding::Item(_) | Binding::Opaque)) {
                map.insert(name.clone(), binding.clone());
            }
        }
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
}

impl Resolved {
    fn join(self, rest: &[String]) -> Self {
        match self {
            Self::Path(mut path) => {
                path.extend(rest.iter().cloned());
                Self::Path(path)
            }
            other => other,
        }
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
        self.modules.insert(module.to_vec(), scope);
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

    fn relative(&self, path: &Path) -> String {
        let path = path.strip_prefix(self.krate.root).unwrap_or(path);
        path.components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                Component::RootDir => Some(String::new()),
                _ => None,
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
            if self.krate.cfg.enabled(item_attrs(item)) == Truth::False {
                continue;
            }
            let ident = match item {
                Item::Use(item) => {
                    bind_use(
                        &item.tree,
                        &mut Vec::new(),
                        item.leading_colon.is_some(),
                        &mut scope,
                    );
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
                scope.define(*namespace, name.clone(), binding.clone());
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
    /// `module` or in the module itself.
    fn lookup<'s>(
        &'s self,
        module: &[String],
        blocks: &'s [Scope],
        namespace: Namespace,
        name: &str,
    ) -> Option<&'s Binding> {
        blocks
            .iter()
            .rev()
            .chain(self.modules.get(module))
            .find_map(|scope| scope.get(namespace, name))
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
        let first_namespace = self::namespace(path, 0, namespace);
        if !leading
            && let Some(binding) = self.lookup(module, blocks, first_namespace, first)
            && !is_use_of(binding, path)
        {
            let resolved = self
                .binding(module, blocks, first_namespace, binding, depth)
                .join(rest);
            return self.followed(resolved, namespace, depth);
        }
        match self.extern_crate(first) {
            Some(krate) => Resolved::Path(vec![krate]).join(rest),
            None => Resolved::Unknown,
        }
    }

    /// Resolves a 2015 `use` path, which starts at the crate root.
    fn root_path(&self, path: &[String], namespace: Namespace, depth: usize) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        let root = [self.krate.name.to_owned()];
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some(binding) = self.lookup(&root, &[], first_namespace, first)
            && !is_use_of(binding, path)
        {
            let resolved = self
                .binding(&root, &[], first_namespace, binding, depth)
                .join(rest);
            return self.followed(resolved, namespace, depth);
        }
        if let Some(krate) = self.extern_crate(first) {
            return Resolved::Path(vec![krate]).join(rest);
        }
        let mut full = root.to_vec();
        full.extend(path.iter().cloned());
        self.follow(full, namespace, depth)
    }

    fn followed(&self, resolved: Resolved, namespace: Namespace, depth: usize) -> Resolved {
        match resolved {
            Resolved::Path(path) => self.follow(path, namespace, depth),
            other => other,
        }
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
        if path.first().map(String::as_str) != Some(self.krate.name) || depth > MAX_DEPTH {
            return Resolved::Path(path);
        }
        for end in 1..path.len() {
            let (Some(module), Some(name)) = (path.get(..end), path.get(end)) else {
                break;
            };
            let Some(scope) = self.modules.get(module) else {
                // A type or a function, whose items are its own.
                break;
            };
            let name_namespace = self::namespace(&path, end, namespace);
            match scope.get(name_namespace, name) {
                Some(binding @ Binding::Use { .. }) => {
                    let rest = path.get(end + 1..).unwrap_or_default();
                    let resolved = self
                        .binding(module, &[], name_namespace, binding, depth + 1)
                        .join(rest);
                    return self.followed(resolved, namespace, depth + 1);
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
            if let Resolved::Path(definition) = self.step(path.to_vec(), Namespace::Values, 0)
                && definition != path
            {
                definitions.insert(definition, entry);
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
        if let Some(binding) = self.lookup(module, blocks, first_namespace, first) {
            let resolved = self
                .binding(module, blocks, first_namespace, binding, 0)
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
fn bind_use(tree: &UseTree, prefix: &mut Vec<String>, leading: bool, scope: &mut Scope) {
    let mut bind = |name: String, path: Vec<String>| {
        if name != "_" {
            scope.import(name, Binding::Use { path, leading });
        }
    };
    match tree {
        UseTree::Path(tree) => {
            prefix.push(tree.ident.unraw().to_string());
            bind_use(&tree.tree, prefix, leading, scope);
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
                bind_use(tree, prefix, leading, scope);
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
        match resolved {
            Resolved::Path(path) => Resolved::Path(canonical(path)),
            other => other,
        }
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
                    block.define(Namespace::Values, name, Binding::Opaque);
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
                        block.define(Namespace::Values, name, Binding::Opaque);
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

    /// Visits code with the names some patterns bind in scope, such as a
    /// function's parameters or a match arm's pattern.
    fn with_patterns<'p>(
        &mut self,
        patterns: impl IntoIterator<Item = &'p Pat>,
        visit: impl FnOnce(&mut Self),
    ) {
        let mut scope = Scope::default();
        for name in pattern_names(patterns) {
            scope.define(Namespace::Values, name, Binding::Opaque);
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
        // A `macro_rules!` definition, or an invocation whose body isn't
        // items.
        self.tokens(item.mac.tokens.clone());
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if is_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_patterns(parameters(&item.sig), |this| {
            visit::visit_item_fn(this, item);
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if item.unsafety.is_some() {
            self.scanner.scan.unsafe_code.impls += 1;
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if item.unsafety.is_some() {
            self.scanner.scan.unsafe_code.traits += 1;
        }
        visit::visit_item_trait(self, item);
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
        self.with_patterns(parameters(&item.sig), |this| {
            visit::visit_impl_item_fn(this, item);
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
        self.with_patterns(parameters(&item.sig), |this| {
            visit::visit_trait_item_fn(this, item);
        });
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
        self.visit_expr(&expr.expr);
        self.with_patterns([&*expr.pat], |this| this.visit_block(&expr.body));
    }

    fn visit_expr_if(&mut self, expr: &'ast ExprIf) {
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.then_branch);
        self.blocks.pop();
        if let Some((_, otherwise)) = &expr.else_branch {
            self.visit_expr(otherwise);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast ExprWhile) {
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.body);
        self.blocks.pop();
    }

    fn visit_expr_unsafe(&mut self, expr: &'ast ExprUnsafe) {
        self.scanner.scan.unsafe_code.blocks += 1;
        visit::visit_expr_unsafe(self, expr);
    }

    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        if let Resolved::Path(path) = self.resolve(expr)
            && let Some(entry) = self.scanner.entry(&path)
        {
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
            let unresolved = match self.resolve(func) {
                // A path into a crate no entry names may be a re-export of
                // a disallowed item, as `smol::Executor` is
                // `async_executor::Executor`.
                Resolved::Path(path) => {
                    self.scanner.entry(&path).is_none()
                        && path
                            .first()
                            .is_none_or(|krate| !disallowed.names_crate(krate))
                }
                Resolved::Local => false,
                Resolved::Unknown => true,
            };
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
