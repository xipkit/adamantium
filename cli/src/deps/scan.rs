//! Reading one dependency's library with syn: its `unsafe` code, and its
//! uses of the paths `clippy.toml` disallows.
//!
//! The scan walks the module tree from the library's root, as the compiler
//! does, so tests, examples, benches and files no module declares are left
//! out. syn sees syntax, not resolved names, so paths are resolved as far as
//! the crate's source shows them: through each module's `use` and `extern
//! crate` items, `crate`, `self` and `super`, re-exports between the crate's
//! modules, its dependencies' names and the standard prelude. A module-level
//! call of the crate's own `macro_rules!`, as `crate_root!();` or Tokio's
//! `feature! { #![unix] ... }`, is read as its expansion where its rules can
//! be matched without compiling the crate, and so is a call of a dependency's
//! `#[macro_export]` macro that a `#[macro_use] extern crate` brings in, as
//! stacker's `psm_stack_manipulation!`. A call that this can't tie to a
//! path, such as a method call on a value, is counted as unresolved under
//! its name rather than guessed at.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, mem,
    path::{Component, Path, PathBuf},
    slice,
};

use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use serde::Serialize;
use syn::{
    Arm, AttrStyle, Attribute, Block, Expr, ExprCall, ExprClosure, ExprForLoop, ExprIf, ExprLit,
    ExprMethodCall, ExprPath, ExprUnsafe, ExprWhile, Field, FieldValue, File, FnArg, GenericParam,
    Generics, ImplItem, ImplItemFn, Item, ItemFn, ItemForeignMod, ItemImpl, ItemMacro, ItemMod,
    ItemTrait, Lit, Macro, MacroDelimiter, Meta, MetaList, Pat, PatIdent, Safety, Signature, Stmt,
    Token, TraitItem, TraitItemFn, Type, UseTree, Variant, braced,
    ext::IdentExt,
    parse::{ParseStream, Parser},
    punctuated::Punctuated,
    token,
    visit::{self, Visit},
    visit_mut::{self, VisitMut},
};
use toml_edit::DocumentMut;

use super::{
    cfg::{Cfg, Truth},
    disallowed::{Disallowed, canonical},
    rules::Rules,
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
    /// The dependencies whose source the scan can read, by the name the
    /// crate's code uses, for the macros a `#[macro_use] extern crate`
    /// brings in from them.
    pub dependencies: &'a BTreeMap<String, &'a Crate<'a>>,
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
    /// `unsafe` in macro bodies that syn can't parse as code, such as the
    /// `macro_rules!` definitions the scan doesn't expand where they are
    /// called, and the attributes `unsafe_code` reports
    /// that are written there without `unsafe(...)`.
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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
    let mut scanner = Scanner::new(krate, disallowed);
    scanner.scan.forbids_unsafe = manifest_forbids_unsafe(krate.root);
    // Every module's names are bound before any code is read, so a path can
    // be followed through another module's `use` items.
    let root = vec![krate.name.to_owned()];
    scanner.load(krate.src, root, true, None, &mut Textual::default());
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
            for attr in &loaded.attrs {
                visitor.visit_attribute(attr);
            }
            for item in &loaded.items {
                visitor.visit_item(item);
            }
        }
        visited.extend(files);
    }
    // A macro expanded where some calls are may have calls the scan can't
    // expand, such as in functions, whose code it can't otherwise see: its
    // body is read as tokens then, as a macro's that isn't expanded is.
    loop {
        let ready: Vec<_> = scanner
            .deferred
            .iter()
            .filter(|(_, deferred)| scanner.unexpanded.contains(&deferred.name))
            .map(|(at, _)| at.clone())
            .collect();
        if ready.is_empty() {
            break;
        }
        for at in ready {
            let Some(deferred) = scanner.deferred.remove(&at) else {
                continue;
            };
            let mut visitor = FileVisitor {
                scanner: &mut scanner,
                file: at.0,
                module: deferred.module,
                mod_dir: deferred.mod_dir,
                path_dir: deferred.path_dir,
                blocks: Vec::new(),
                attributes: HashSet::new(),
            };
            visitor.tokens(deferred.tokens);
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Whether a `use` may bind `name` here, as it may bind a macro.
    fn imports(&self, name: &str) -> bool {
        [&self.types, &self.values]
            .into_iter()
            .filter_map(|map| map.get(name))
            .any(|bound| {
                bound
                    .binding
                    .alternatives()
                    .iter()
                    .any(|binding| matches!(binding, Binding::Use { .. }))
            })
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
    /// What a path through a name with more than one binding may mean. With
    /// no alternatives, it is a way to a `use` that leads back to it, which
    /// means nothing.
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

/// A parsed file of the module tree, or the items a `macro_rules!` call
/// expands to. The calls of the crate's `macro_rules!` that are expanded
/// aren't among its items.
struct Loaded {
    /// The file's inner attributes; an expansion has none.
    attrs: Vec<Attribute>,
    items: Vec<Item>,
    /// Its name relative to the package. An expansion's is the name of the
    /// file its macro is written in; its spans may be in the call's file
    /// too.
    rel: String,
    module: Vec<String>,
    /// Where its child modules' files are.
    mod_dir: PathBuf,
    /// What its `#[path]` attributes are relative to.
    path_dir: PathBuf,
}

/// A file a module's code may be in.
struct ModuleFile {
    path: PathBuf,
    mod_rs: bool,
    /// The predicate of the `cfg`s that make a build take the module from
    /// this file, where it may be in another.
    condition: Option<TokenStream>,
}

/// Where a module's code is.
enum Location {
    Inline {
        dir: PathBuf,
    },
    /// The files a build may take it from.
    Files(Vec<ModuleFile>),
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
    /// The `macro_rules!` expanded where they are called, by file and where
    /// their names are written.
    expanded: HashSet<(String, usize, usize)>,
    /// The names of the macros called where the scan doesn't expand them.
    unexpanded: HashSet<String>,
    /// The bodies of the expanded `macro_rules!` that are read as tokens
    /// only if a call of their name isn't expanded, by file and where their
    /// names are written.
    deferred: BTreeMap<(String, usize, usize), Deferred>,
    /// The files read, relative to the package, by the source their spans
    /// are in, as [`Span::file`] names it. A macro's expansion has tokens
    /// from the call and from the macro, which may be in different files.
    sources: HashMap<String, String>,
    /// How many calls of `macro_rules!` have been expanded.
    expansions: usize,
    /// The macros that `#[macro_use] extern crate` items bring in from
    /// other crates, which a call names where no other macro of its name is
    /// in scope.
    prelude: Vec<MacroRules>,
    /// The sources, as [`Span::file`] names them, of the files read for
    /// other crates' macros. Code written there counts for the crate that
    /// wrote it, not for this one, where the macros are called.
    foreign: HashSet<String>,
    /// Where `$crate` is written in other crates' macros, by source, as
    /// [`Span::file`] names it, line and column, mapped to the name the
    /// crate goes by here.
    dollar_crates: HashMap<(String, usize, usize), Ident>,
    /// The `#[macro_export]` macros defined so far, where the crate is read
    /// for another crate that brings them in.
    exports: Option<Vec<Export>>,
    /// Where `$crate` is written in the crate's macros, where the crate is
    /// read for another crate that brings them in.
    crate_spans: Vec<Span>,
}

/// A `#[macro_export]` macro of a crate, as another crate brings it in.
struct Export {
    name: String,
    rules: Option<Rules>,
    /// The file it is written in, relative to the package of the scanner
    /// that has it.
    rel: String,
    /// Where its name is written.
    span: Span,
    /// Whether the crate's build defines and exports it, by the `cfg`s of
    /// that build.
    defined: Truth,
}

/// The body of an expanded `macro_rules!`, and the module it is written in.
struct Deferred {
    name: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
    tokens: TokenStream,
}

/// A `macro_rules!` in textual scope.
struct MacroRules {
    name: String,
    /// Its rules, if the scan can read them. A macro whose rules it can't
    /// read isn't expanded, nor is any call it may shadow.
    rules: Option<Rules>,
    /// The file it is written in.
    rel: String,
    /// Where its name is written.
    span: Span,
    /// The predicates of the `cfg`s it is defined under: its own, and those
    /// of the modules around it.
    condition: Vec<TokenStream>,
    /// Whether every build defines it.
    certain: bool,
    /// Whether a build that `condition` holds for defines it: not known
    /// where it is defined in the expansion of another crate's macro, or is
    /// another crate's, whose build decides.
    defined: Truth,
    /// For another crate's macro, which of the crate root's `#[macro_use]
    /// extern crate` items brings it in, by their order: the last that
    /// brings in a name hides the ones before it.
    import: Option<usize>,
}

/// The `macro_rules!` a call can name by its bare name at a place in the
/// module tree, as rustc's textual scope has them: the ones defined before
/// it in its module and the modules around it, and in the `#[macro_use]`
/// modules declared before it in those, in order.
#[derive(Default)]
struct Textual {
    macros: Vec<MacroRules>,
    /// The predicates of the `cfg`s on the modules around the place.
    conditions: Vec<TokenStream>,
    /// How many expansions the place is inside.
    depth: usize,
    /// Whether a build may not have the place for a reason its conditions
    /// don't show: it is in the expansion of another crate's macro, which
    /// that crate's build may not define.
    uncertain: bool,
}

/// Where items being indexed are.
#[derive(Clone, Copy)]
struct Place<'p> {
    /// The module they are in.
    module: &'p [String],
    /// Where the module's `mod name;` files are.
    mod_dir: &'p Path,
    /// What the module's `#[path]` attributes are relative to.
    path_dir: &'p Path,
    /// The file they are written in, relative to the package.
    rel: &'p str,
}

/// How many `macro_rules!` expansions deep the scan follows calls, so that a
/// macro that calls itself can't keep it going: rustc's default
/// `recursion_limit`.
const MAX_EXPANSIONS: usize = 128;

/// How many calls of `macro_rules!` the scan expands in one crate, so that
/// a macro whose expansion calls it more than once can't keep it going.
const MAX_EXPANDED_CALLS: usize = 10_000;

impl<'a> Scanner<'a> {
    fn new(krate: &'a Crate<'a>, disallowed: &'a Disallowed) -> Self {
        Self {
            krate,
            disallowed,
            scan: Scan::default(),
            visited: HashSet::new(),
            files: Vec::new(),
            modules: HashMap::new(),
            definitions: HashMap::new(),
            expanded: HashSet::new(),
            unexpanded: HashSet::new(),
            deferred: BTreeMap::new(),
            sources: HashMap::new(),
            expansions: 0,
            prelude: Vec::new(),
            foreign: HashSet::new(),
            dollar_crates: HashMap::new(),
            exports: None,
            crate_spans: Vec::new(),
        }
    }

    /// Loads the file of the module at `module`, and the modules it declares.
    /// A mod-rs file, such as the crate root, a `mod.rs` or a file named by
    /// `#[path]`, keeps its child modules' files beside it; any other
    /// `name.rs` keeps them in `name/`. `from` is the declaration that names
    /// the file, and `textual` the macros in scope there. Returns the file's
    /// inner attributes, which are its module's, or none if it isn't read.
    fn load(
        &mut self,
        path: &Path,
        module: Vec<String>,
        mod_rs: bool,
        from: Option<(&str, Span)>,
        textual: &mut Textual,
    ) -> Vec<Attribute> {
        let rel = self.relative(path);
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let reason = format!("reading {rel}: {error}");
                self.unscanned(match from {
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
                return Vec::new();
            }
        };
        if !self.visited.insert(path.to_path_buf()) {
            return Vec::new();
        }
        let mut file = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(error) => {
                self.unscanned(Unscanned {
                    file: rel,
                    line: Some(error.span().start().line),
                    reason: format!("syn could not parse it: {error}"),
                });
                return Vec::new();
            }
        };
        let mut first = FirstSpan(None);
        first.visit_file(&file);
        if let FirstSpan(Some(span)) = first {
            self.sources.insert(span.file(), rel.clone());
        }
        if self.krate.cfg.enabled(&file.attrs) == Truth::False {
            return Vec::new();
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
        let outer = textual.conditions.len();
        textual.conditions.extend(cfg_predicates(&file.attrs));
        if module.len() == 1 {
            self.import_macros(&file.items, textual);
        }
        let mut scope = Scope::default();
        let place = Place {
            module: &module,
            mod_dir: &mod_dir,
            path_dir: &dir,
            rel: &rel,
        };
        self.index(&mut file.items, place, &mut scope, textual);
        textual.conditions.truncate(outer);
        self.add_scope(&module, scope);
        let attrs = file.attrs.clone();
        self.files.push(Loaded {
            attrs: file.attrs,
            items: file.items,
            rel,
            module,
            mod_dir,
            path_dir: dir,
        });
        attrs
    }

    /// Binds the names `items` define in the module at `place` into
    /// `scope`, and reads what they bring into the module tree, in the order
    /// they are written: the modules they declare, and the expansions of
    /// their calls of `macro_rules!` in `textual`, which their own
    /// `macro_rules!` join. A call that is expanded is taken out of the
    /// items, so a call left among them isn't.
    fn index(
        &mut self,
        items: &mut Vec<Item>,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) {
        self.bind(scope, items.iter(), Some(place.module), textual.uncertain);
        let mut indexed = Vec::with_capacity(items.len());
        for mut item in mem::take(items) {
            match &mut item {
                Item::Mod(item) => self.child(item, place, textual),
                Item::Macro(call) => match &call.ident {
                    Some(name) => self.define(call, name, place.rel, textual),
                    None => {
                        if self.expand(call, place, scope, textual) {
                            continue;
                        }
                    }
                },
                _ => {}
            }
            indexed.push(item);
        }
        *items = indexed;
    }

    /// Adds the names a module binds to those already bound for its path.
    fn add_scope(&mut self, module: &[String], scope: Scope) {
        match self.modules.get_mut(module) {
            Some(existing) => existing.merge(scope),
            None => {
                self.modules.insert(module.to_vec(), scope);
            }
        }
    }

    /// Reads a module that the module at `place` declares. Its
    /// `macro_rules!` stay in textual scope after it if it is
    /// `#[macro_use]`, by an outer attribute or an inner one.
    fn child(&mut self, item: &mut ItemMod, place: Place, textual: &mut Textual) {
        if self.krate.cfg.enabled(&item.attrs) == Truth::False {
            return;
        }
        let mut child = place.module.to_vec();
        child.push(item.ident.unraw().to_string());
        let macros = textual.macros.len();
        let conditions = textual.conditions.len();
        textual.conditions.extend(cfg_predicates(&item.attrs));
        match self.locate(item, place.mod_dir, place.path_dir) {
            // syn gives an inline module's inner attributes with its outer ones.
            Location::Inline { dir } => {
                if let Some((_, items)) = &mut item.content {
                    let mut scope = Scope::default();
                    let inner = Place {
                        module: &child,
                        mod_dir: &dir,
                        path_dir: &dir,
                        rel: place.rel,
                    };
                    self.index(items, inner, &mut scope, textual);
                    self.add_scope(&child, scope);
                }
                self.keep_macros(&item.attrs, macros, textual);
            }
            // Each file may have its own `#![macro_use]`.
            Location::Files(files) => {
                let declared = self.file_of(item.ident.span(), place.rel);
                for file in files {
                    let from = Some((declared.as_str(), item.ident.span()));
                    let outer = textual.conditions.len();
                    let macros = textual.macros.len();
                    textual.conditions.extend(file.condition);
                    let mut attrs = item.attrs.clone();
                    attrs.extend(self.load(&file.path, child.clone(), file.mod_rs, from, textual));
                    textual.conditions.truncate(outer);
                    self.keep_macros(&attrs, macros, textual);
                }
            }
            Location::Unscannable(reason) => self.unscanned(Unscanned {
                file: self.file_of(item.ident.span(), place.rel),
                line: Some(item.ident.span().start().line),
                reason,
            }),
        }
        textual.conditions.truncate(conditions);
    }

    /// Keeps the `macro_rules!` a module defined, those in `textual` from
    /// `start` on, in textual scope after it if its attributes, outer and
    /// inner, make it `#[macro_use]`.
    fn keep_macros(&self, attrs: &[Attribute], start: usize, textual: &mut Textual) {
        match self.macro_use(attrs) {
            Truth::True => {}
            // Where the module may not be `#[macro_use]`, a call its macros
            // may shadow can't be expanded.
            Truth::Unknown => {
                for definition in textual.macros.iter_mut().skip(start) {
                    definition.rules = None;
                }
            }
            Truth::False => textual.macros.truncate(start),
        }
    }

    /// Whether a module's attributes make it `#[macro_use]`.
    fn macro_use(&self, attrs: &[Attribute]) -> Truth {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .filter(|(_, meta)| matches!(meta, Meta::Path(path) if path.is_ident("macro_use")))
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies))
    }

    /// Lists source the scan couldn't read, once: a call in a macro may be
    /// expanded many times.
    fn unscanned(&mut self, unscanned: Unscanned) {
        if !self.scan.unscanned.contains(&unscanned) {
            self.scan.unscanned.push(unscanned);
        }
    }

    /// The file, relative to the package, that the token at `span` is
    /// written in, or `fallback` if it isn't in a file the scan read.
    fn file_of(&self, span: Span, fallback: &str) -> String {
        self.sources
            .get(&span.file())
            .map_or_else(|| fallback.to_owned(), Clone::clone)
    }

    /// Brings a `macro_rules!` named `name`, read from the code of `rel`,
    /// into textual scope, and notes it if the crate is read for its
    /// exports and it is `#[macro_export]`.
    fn define(&mut self, item: &ItemMacro, name: &Ident, rel: &str, textual: &mut Textual) {
        let mut condition = textual.conditions.clone();
        condition.extend(cfg_predicates(&item.attrs));
        let span = Span::call_site();
        let holds = self
            .krate
            .cfg
            .holds(cfg_call("all", condition.clone(), span));
        if holds == Truth::False {
            return;
        }
        let rel = self.file_of(name.span(), rel);
        let rules = Rules::parse(item.mac.tokens.clone());
        if let Some(exports) = &mut self.exports {
            // A `$crate` the crate's macros write may end up in an exported
            // macro another one defines, as `crate`.
            self.crate_spans
                .extend(rules.iter().flat_map(Rules::crate_spans));
            let effective = self.krate.cfg.effective(&item.attrs);
            let export = effective
                .iter()
                .filter(|(_, meta)| meta.path().is_ident("macro_export"));
            let exported = export
                .clone()
                .fold(Truth::False, |truth, (applies, _)| truth.or(*applies));
            // Where it may be, the calls its rules write by bare names are
            // read as calls of the crate's macros, which aren't expanded.
            let local_inner_macros = export.clone().any(|(_, meta)| {
                matches!(meta, Meta::List(list) if list
                    .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
                    .is_ok_and(|args| args.iter().any(|arg| arg == "local_inner_macros")))
            });
            if exported != Truth::False {
                let mut rules = Rules::parse(item.mac.tokens.clone());
                if local_inner_macros && let Some(rules) = &mut rules {
                    rules.local_inner_macros();
                    self.crate_spans.extend(rules.crate_spans());
                }
                exports.push(Export {
                    name: name.unraw().to_string(),
                    rules,
                    rel: rel.clone(),
                    span: name.span(),
                    defined: holds.and(exported),
                });
            }
        }
        textual.macros.push(MacroRules {
            name: name.unraw().to_string(),
            rules,
            rel,
            span: name.span(),
            condition,
            certain: holds == Truth::True && !textual.uncertain,
            defined: if textual.uncertain {
                Truth::Unknown
            } else {
                Truth::True
            },
            import: None,
        });
    }

    /// Brings into the prelude the macros that the `#[macro_use] extern
    /// crate` items among the crate root's `items` bring in from the
    /// dependencies whose source can be read: their `#[macro_export]`
    /// macros, or the ones a `#[macro_use(...)]` names. rustc takes such an
    /// item only at the crate root, and its macros for the whole crate,
    /// before and after it. `textual` has the root's conditions.
    fn import_macros(&mut self, items: &[Item], textual: &Textual) {
        for (import, item) in items.iter().enumerate() {
            let Item::ExternCrate(item) = item else {
                continue;
            };
            let Some(dependency) = self.krate.dependencies.get(&item.ident.unraw().to_string())
            else {
                continue;
            };
            let mut condition = textual.conditions.clone();
            condition.extend(cfg_predicates(&item.attrs));
            let holds = self
                .krate
                .cfg
                .holds(cfg_call("all", condition.clone(), Span::call_site()));
            let macro_use = self
                .krate
                .cfg
                .effective(&item.attrs)
                .iter()
                .any(|(applies, meta)| {
                    *applies != Truth::False && meta.path().is_ident("macro_use")
                });
            if holds == Truth::False || !macro_use {
                continue;
            }
            let (exports, crate_spans) = self.exports_of(dependency);
            for span in crate_spans {
                let start = span.start();
                self.dollar_crates
                    .insert((span.file(), start.line, start.column), item.ident.clone());
            }
            for export in exports {
                let imported = self.imported(&item.attrs, &export.name);
                if imported == Truth::False || export.defined == Truth::False {
                    continue;
                }
                let defined = export.defined.and(imported);
                self.prelude.push(MacroRules {
                    name: export.name,
                    rules: export.rules,
                    rel: export.rel,
                    span: export.span,
                    condition: condition.clone(),
                    certain: holds == Truth::True && defined == Truth::True,
                    defined,
                    import: Some(import),
                });
            }
        }
    }

    /// Whether an `extern crate` item with these attributes brings in the
    /// macro `name`: whether it is `#[macro_use]`, or a `#[macro_use(...)]`
    /// that names it.
    fn imported(&self, attrs: &[Attribute], name: &str) -> Truth {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .filter(|(_, meta)| match meta {
                Meta::Path(path) => path.is_ident("macro_use"),
                Meta::List(list) => {
                    list.path.is_ident("macro_use")
                        && list
                            .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
                            .is_ok_and(|names| names.iter().any(|named| named.unraw() == name))
                }
                Meta::NameValue(_) => false,
            })
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies))
    }

    /// The `#[macro_export]` macros of a dependency, read from its module
    /// tree with the `cfg`s of its own build, and where `$crate` is written
    /// in its macros. A procedural macro crate can't export any. The
    /// dependency's files are noted as other crates'.
    fn exports_of(&mut self, krate: &Crate) -> (Vec<Export>, Vec<Span>) {
        if krate.cfg.proc_macro {
            return (Vec::new(), Vec::new());
        }
        let mut reader = Scanner::new(krate, self.disallowed);
        reader.exports = Some(Vec::new());
        let root = vec![krate.name.to_owned()];
        reader.load(krate.src, root, true, None, &mut Textual::default());
        for (source, rel) in reader.sources {
            self.foreign.insert(source.clone());
            self.sources
                .insert(source, self.relative(&krate.root.join(rel)));
        }
        let mut exports = reader.exports.unwrap_or_default();
        for export in &mut exports {
            export.rel = self.relative(&krate.root.join(&export.rel));
        }
        (exports, reader.crate_spans)
    }

    /// The definitions a call of `name` may expand, the latest first: those
    /// in textual scope back to the first that every build defines, then,
    /// where a build may define none of them, those in the prelude, by the
    /// `extern crate` items that bring them in, back to the first that
    /// every build brings in. One crate's definitions of a name don't hide
    /// each other, since its build has at most one. A macro that a `use` in
    /// the calling module brings in would come before the prelude's, so a
    /// call that a `use` or glob import there may name isn't given them.
    /// `None` if there is no definition, or the rules of one of them can't
    /// be read.
    fn candidates<'t>(
        &'t self,
        textual: &'t Textual,
        scope: &Scope,
        name: &str,
    ) -> Option<Vec<(&'t MacroRules, &'t Rules)>> {
        let mut candidates = Vec::new();
        for definition in textual.macros.iter().rev() {
            if definition.name != name {
                continue;
            }
            candidates.push((definition, definition.rules.as_ref()?));
            if definition.certain {
                return Some(candidates);
            }
        }
        let mut prelude = self
            .prelude
            .iter()
            .filter(|definition| definition.name == name)
            .peekable();
        if prelude.peek().is_some() {
            if scope.globs || scope.imports(name) {
                return None;
            }
            let mut hidden = None;
            for definition in prelude.rev() {
                if hidden.is_some_and(|import| definition.import != Some(import)) {
                    break;
                }
                candidates.push((definition, definition.rules.as_ref()?));
                if definition.certain {
                    hidden = definition.import;
                }
            }
        }
        (!candidates.is_empty()).then_some(candidates)
    }

    /// Expands a call of a `macro_rules!` in textual scope where it is
    /// written, as `crate_root!();` or `feature! { #![unix] mod symlink; }`,
    /// and returns whether it did. Each definition the call may name expands
    /// under the `cfg`s that select it: its own, and none of the later
    /// ones'. A call that one of them can't be matched to or doesn't expand
    /// to items, or that is past a limit, isn't expanded. The expansion is
    /// part of the calling module, but it is read as its own [`Loaded`], so
    /// that its code counts for each call.
    fn expand(
        &mut self,
        call: &ItemMacro,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) -> bool {
        let Some(name) = call.mac.path.get_ident() else {
            return false;
        };
        if self.krate.cfg.enabled(&call.attrs) == Truth::False {
            return false;
        }
        let Some(candidates) = self.candidates(textual, scope, &name.unraw().to_string()) else {
            return false;
        };
        let file = self.file_of(name.span(), place.rel);
        let start = name.span().start();
        // A call past a limit is read as tokens.
        let limit = if textual.depth == MAX_EXPANSIONS {
            Some(format!(
                "`{name}!` expands to more than {MAX_EXPANSIONS} nested macro calls"
            ))
        } else if self.expansions == MAX_EXPANDED_CALLS {
            Some(format!(
                "the crate's macros expand to more than {MAX_EXPANDED_CALLS} calls"
            ))
        } else {
            None
        };
        if let Some(reason) = limit {
            self.unscanned(Unscanned {
                file,
                line: Some(start.line),
                reason,
            });
            return false;
        }
        let call_condition = cfg_predicates(&call.attrs);
        // The predicates under which a later definition is defined, and,
        // for the `extern crate` item whose definitions are being read, its
        // own: they don't hide each other, only the earlier items'.
        let mut later = Vec::new();
        let mut import: Option<(Option<usize>, Option<TokenStream>)> = None;
        let mut expansions = Vec::new();
        for (candidate, rules) in candidates {
            // The expansion is visited apart from other files, and no
            // attribute starts at a macro's name, so the `cfg` it is given
            // is told apart from those written in it.
            let span = candidate.span;
            if candidate.import.is_none()
                || import
                    .as_ref()
                    .is_some_and(|(current, _)| *current != candidate.import)
            {
                later.extend(import.take().and_then(|(_, hides)| hides));
            }
            let mut condition = call_condition.clone();
            condition.extend(candidate.condition.iter().cloned());
            if !later.is_empty() {
                condition.push(cfg_call(
                    "not",
                    [cfg_call("any", later.clone(), span)],
                    span,
                ));
            }
            // A definition hides the earlier ones only where its condition
            // shows it is defined.
            let hides = (candidate.defined == Truth::True)
                .then(|| cfg_call("all", candidate.condition.clone(), span));
            match candidate.import {
                None => later.extend(hides),
                Some(_) => {
                    let (_, item) = import.get_or_insert((candidate.import, None));
                    if hides.is_some() {
                        *item = hides;
                    }
                }
            }
            let defined = candidate.defined;
            let predicate = cfg_call("all", condition.clone(), span);
            if self.krate.cfg.holds(predicate.clone()).and(defined) == Truth::False {
                continue;
            }
            let Some(mut items) = rules
                .expand(call.mac.tokens.clone())
                .and_then(|tokens| parse_items.parse2(tokens).ok())
            else {
                return false;
            };
            Expand.fragment(&mut items);
            DollarCrates(&self.dollar_crates).items(&mut items);
            if !condition.is_empty() {
                let cfg = cfg_attribute(predicate, span);
                for item in &mut items {
                    if let Some(attrs) = item_attrs_mut(item) {
                        let own = mem::take(attrs);
                        attrs.push(cfg.clone());
                        attrs.extend(own);
                    }
                }
            }
            let start = span.start();
            let at = (candidate.rel.clone(), start.line, start.column);
            expansions.push((items, at, candidate.import.is_some(), defined));
        }
        self.expansions += 1;
        for (mut items, at, foreign, defined) in expansions {
            let rel = at.0.clone();
            // Another crate's macro isn't read as tokens here in any case.
            if !foreign {
                self.expanded.insert(at);
            }
            let inner = Place { rel: &rel, ..place };
            textual.depth += 1;
            let uncertain = textual.uncertain;
            textual.uncertain |= defined != Truth::True;
            self.index(&mut items, inner, scope, textual);
            textual.uncertain = uncertain;
            textual.depth -= 1;
            self.files.push(Loaded {
                attrs: Vec::new(),
                items,
                rel,
                module: place.module.to_vec(),
                mod_dir: place.mod_dir.to_path_buf(),
                path_dir: place.path_dir.to_path_buf(),
            });
        }
        true
    }

    /// Where a module's code is. Where its `#[path]` depends on a `cfg` the
    /// scan can't know, a module declared with `mod name;` may be in the
    /// file the path names or where it would be without it: each of those
    /// that has a file may be the one built, and one that has none can't be.
    fn locate(&self, item: &ItemMod, mod_dir: &Path, path_dir: &Path) -> Location {
        let name = item.ident.unraw().to_string();
        let span = item.ident.span();
        // Each place a build may take the module from, `None` for where it
        // is without a `#[path]`, with the predicate that selects it: of
        // the `#[path]`s that apply, the last is the one taken.
        let mut places = Vec::new();
        let mut later = Vec::new();
        for (conditions, meta) in conditional(&item.attrs).into_iter().rev() {
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
            let applies = cfg_call("all", conditions, span);
            let none_later = cfg_call("not", [cfg_call("any", later.clone(), span)], span);
            places.push((
                Some(lit.value()),
                cfg_call("all", [applies.clone(), none_later], span),
            ));
            later.push(applies);
        }
        places.push((None, cfg_call("not", [cfg_call("any", later, span)], span)));
        places.retain(|(_, selects)| self.krate.cfg.holds(selects.clone()) != Truth::False);
        let default = || {
            [
                (mod_dir.join(format!("{name}.rs")), false),
                (mod_dir.join(&name).join("mod.rs"), true),
            ]
            .into_iter()
            .find(|(path, _)| path.is_file())
        };
        let file = |(path, mod_rs), condition| ModuleFile {
            path,
            mod_rs,
            condition,
        };
        match (&item.content, &places[..]) {
            (Some(_), [(path, _)]) => Location::Inline {
                dir: match path {
                    Some(path) => path_dir.join(path),
                    None => mod_dir.join(&name),
                },
            },
            (Some(_), _) => Location::Unscannable(format!(
                "module `{name}` has a #[path] that depends on an unknown cfg"
            )),
            (None, [(Some(path), _)]) => {
                Location::Files(vec![file((path_dir.join(path), true), None)])
            }
            (None, [(None, _)]) => default().map_or_else(
                || {
                    Location::Unscannable(format!(
                        "module `{name}` has no file: neither {name}.rs nor {name}/mod.rs"
                    ))
                },
                |found| Location::Files(vec![file(found, None)]),
            ),
            (None, places) => {
                let files: Vec<_> = places
                    .iter()
                    .filter_map(|(path, selects)| {
                        let found = match path {
                            Some(path) => {
                                let path = path_dir.join(path);
                                path.is_file().then_some((path, true))
                            }
                            None => default(),
                        };
                        found.map(|found| file(found, Some(selects.clone())))
                    })
                    .collect();
                if files.is_empty() {
                    Location::Unscannable(format!(
                        "module `{name}` has no file at any place its #[path], which depends on an unknown cfg, may put it"
                    ))
                } else {
                    Location::Files(files)
                }
            }
        }
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

    /// Binds the names that `items` define in `scope`. Items of a module
    /// have paths under `module`; items of a block have none. `uncertain` is
    /// whether a build may not have the items whatever their `cfg`s say.
    fn bind<'i>(
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
                    bind_use(&item.tree, &mut Vec::new(), leading, certain, scope);
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

    /// Where the disallowed paths into this crate are defined, mapped to
    /// their entries, so that a path reaching the definition another way
    /// matches too, as Clippy's resolved paths do.
    fn definitions(&self) -> HashMap<Vec<String>, usize> {
        let mut resolver = Resolver::new(self, &[]);
        let mut definitions = HashMap::new();
        for (path, entry) in self.disallowed.paths() {
            if path.first().map(String::as_str) != Some(self.krate.name) {
                continue;
            }
            for definition in resolver
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
}

/// The resolution of one path, or of the disallowed paths into the crate.
///
/// Where names have more than one binding, a path can reach one `use` path,
/// or one path into the crate, many ways: through a chain of names with two
/// bindings each, as many as two to the power of its length. Each is
/// resolved once and remembered, so the work grows with the number of them,
/// not of the ways to them.
///
/// A `use` can't be resolved through itself, so a way to it that leads
/// back to it means nothing, as where `mod statx;` and `pub use
/// statx::statx;` both bind `statx` and either may be built: the `use`
/// means `self::statx::statx`, and the way through the `use` to that, which
/// would mean `self::statx::statx::statx`, is not one. What a resolution
/// found while it cut such a way short holds only within the `use` it
/// reached, so it is remembered only once that `use` is resolved.
struct Resolver<'s, 'a> {
    scanner: &'s Scanner<'a>,
    /// The blocks around the place of the path, innermost last. A `use` in
    /// one of them resolves within the ones around it, which come first, so
    /// a count of them names the blocks it resolves in.
    blocks: &'s [Scope],
    uses: HashMap<UseKey, Resolved>,
    walks: HashMap<WalkKey, Resolved>,
    /// The `use` paths being resolved, outermost first.
    active: Vec<UseSite>,
    /// The outermost of `active` that a way cut short reached, since the
    /// resolution that is under way started.
    circular: Option<usize>,
}

/// A `use` path and the place it is resolved in.
#[derive(Clone, PartialEq, Eq, Hash)]
struct UseSite {
    module: Vec<String>,
    blocks: usize,
    namespace: Namespace,
    path: Vec<String>,
    leading: bool,
}

/// A `use` path to resolve, as `Resolver::use_path` takes it.
#[derive(PartialEq, Eq, Hash)]
struct UseKey {
    site: UseSite,
    depth: usize,
}

/// A path into the crate to follow, as `Resolver::walk` takes it.
#[derive(PartialEq, Eq, Hash)]
struct WalkKey {
    path: Vec<String>,
    start: usize,
    namespace: Namespace,
    depth: usize,
}

impl<'s, 'a> Resolver<'s, 'a> {
    fn new(scanner: &'s Scanner<'a>, blocks: &'s [Scope]) -> Self {
        Self {
            scanner,
            blocks,
            uses: HashMap::new(),
            walks: HashMap::new(),
            active: Vec::new(),
            circular: None,
        }
    }

    /// Runs `resolve`, and says whether what it resolves holds anywhere: not
    /// where it cut short a way to one of the first `outside` of `active`,
    /// which it runs within.
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

    /// Resolves a path written in `module` within the blocks, for its last
    /// segment in `namespace`: values for a path in an expression, types
    /// for the type of a qualified path. A path whose every way leads back
    /// to a `use` on it can't be resolved.
    fn path(
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
        // On its own, `self` is a method's receiver, not the module.
        if first == "self" && rest.is_empty() {
            return Resolved::Local;
        }
        let scanner = self.scanner;
        if let Some(path) = scanner.relative_path(module, path) {
            return self.follow(path, namespace, 0);
        }
        if leading {
            if scanner.krate.edition_2015 {
                return self.root_path(path, namespace, 0);
            }
            return match scanner.extern_crate(first) {
                Some(krate) => Resolved::Path(vec![krate]).join(rest),
                None => Resolved::Unknown,
            };
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
        if let Some(krate) = scanner.extern_crate(first) {
            return Resolved::Path(vec![krate]).join(rest);
        }
        // A glob import shadows the prelude and may provide any name. It
        // could shadow a crate name too, but no crate exports an item named
        // like another crate, while a `Vec` or `Box` of its own is common.
        let globs = self
            .blocks
            .iter()
            .chain(scanner.modules.get(module))
            .any(|scope| scope.globs);
        if !globs && let Some((_, prelude)) = PRELUDE.iter().find(|(name, _)| name == first) {
            return Resolved::Path(prelude.split("::").map(str::to_owned).collect()).join(rest);
        }
        Resolved::Unknown
    }

    /// The binding of `name` in `namespace`, in the first `blocks` of the
    /// blocks around a place in `module` or in the module itself, with the
    /// count of blocks around the scope that binds it: a `use` resolves
    /// there, not where its name is used.
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
        self.scanner
            .modules
            .get(module)
            .and_then(|scope| scope.get(namespace, name))
            .map(|binding| (binding, 0))
    }

    /// Resolves a binding in `namespace`, found in `module` within the first
    /// `blocks` of the blocks.
    fn binding(
        &mut self,
        module: &[String],
        blocks: usize,
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

    /// Resolves a `use` path written in `module` within the first `blocks`
    /// of the blocks, for its target in `namespace`.
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
            return self.follow(path, namespace, depth);
        }
        if scanner.krate.edition_2015 {
            return self.root_path(path, namespace, depth);
        }
        let unbound = |_: &mut Self| match scanner.extern_crate(first) {
            Some(krate) => Resolved::Path(vec![krate]).join(rest),
            None => Resolved::Unknown,
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if !leading
            && let Some((binding, scope)) = self.lookup(module, blocks, first_namespace, first)
        {
            return self.use_binding(module, scope, binding, path, namespace, depth, unbound);
        }
        unbound(self)
    }

    /// Resolves a `use` path written in `module`, whose first segment is
    /// bound to `binding` within the first `blocks` of the blocks. A `use`
    /// can't refer to itself: in `use tokio;`, `tokio` is the crate. So where
    /// the binding is the `use` of the path, the path resolves as `unbound`
    /// says a path whose first segment isn't bound does.
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

    /// Resolves a 2015 `use` path, which starts at the crate root.
    fn root_path(&mut self, path: &[String], namespace: Namespace, depth: usize) -> Resolved {
        let Some((first, rest)) = path.split_first() else {
            return Resolved::Unknown;
        };
        let scanner = self.scanner;
        let root = [scanner.krate.name.to_owned()];
        let unbound = |resolver: &mut Self| {
            if let Some(krate) = scanner.extern_crate(first) {
                return Resolved::Path(vec![krate]).join(rest);
            }
            let mut full = root.to_vec();
            full.extend(path.iter().cloned());
            resolver.follow(full, namespace, depth)
        };
        let first_namespace = self::namespace(path, 0, namespace);
        if let Some((binding, _)) = self.lookup(&root, 0, first_namespace, first) {
            return self.use_binding(&root, 0, binding, path, namespace, depth, unbound);
        }
        unbound(self)
    }

    fn followed(&mut self, resolved: Resolved, namespace: Namespace, depth: usize) -> Resolved {
        resolved.map_paths(|path| self.follow(path, namespace, depth))
    }

    /// Follows a path into this crate through the `use` items of the modules
    /// it passes, as `crate::sync::Mutex` reaches `std::sync::Mutex` through
    /// `mod sync { pub use std::sync::Mutex; }`. It stops at a disallowed
    /// path, which names a re-export as often as a definition.
    fn follow(&mut self, path: Vec<String>, namespace: Namespace, depth: usize) -> Resolved {
        if self.scanner.disallowed.entry(&path).is_some() {
            return Resolved::Path(path);
        }
        self.step(path, namespace, depth)
    }

    fn step(&mut self, path: Vec<String>, namespace: Namespace, depth: usize) -> Resolved {
        self.walk(path, 1, namespace, depth)
    }

    /// Follows a path into this crate from its segment at `start`.
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
            return Resolved::Path(path.to_vec());
        }
        for end in start..path.len() {
            let (Some(module), Some(name)) = (path.get(..end), path.get(end)) else {
                break;
            };
            let Some(scope) = scanner.modules.get(module) else {
                // A type or a function, whose items are its own.
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
        Resolved::Path(path.to_vec())
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

/// Finds the span of an identifier in a file, which names the source that
/// every span in the file is in. A file with no identifier has no site the
/// scan reports.
struct FirstSpan(Option<Span>);

impl<'ast> Visit<'ast> for FirstSpan {
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if self.0.is_none() {
            visit::visit_attribute(self, attr);
        }
    }

    fn visit_item(&mut self, item: &'ast Item) {
        if self.0.is_none() {
            visit::visit_item(self, item);
        }
    }

    fn visit_ident(&mut self, ident: &'ast Ident) {
        self.0.get_or_insert(ident.span());
    }
}

/// Writes each `crate` that another crate's macro wrote for `$crate` as a
/// path to that crate, `::name`, by the name the crate goes by here: in
/// every edition, such a path can start at the crates the build is given.
/// Until the expansion is read as code, `$crate` stays one token, as in
/// rustc, so that a macro it is passed to matches it as an identifier. A
/// `crate` the macro writes itself means the calling crate, and stays.
struct DollarCrates<'d>(&'d HashMap<(String, usize, usize), Ident>);

impl DollarCrates<'_> {
    /// Rewrites `items`, and the items and code in them.
    fn items(&mut self, items: &mut Vec<Item>) {
        self.split(items);
        for item in items {
            self.visit_item_mut(item);
        }
    }

    /// Splits each `use` item among `items` whose root group has a branch
    /// that starts with another crate's `$crate`, as `use {$crate::spawn as
    /// start, other::Thing};`, into one item for each branch, which means
    /// the same: the path to the crate can only start a `use` item.
    fn split(&self, items: &mut Vec<Item>) {
        *items = mem::take(items)
            .into_iter()
            .flat_map(|item| match item {
                Item::Use(item) => self.branches(item).into_iter().map(Item::Use).collect(),
                item => vec![item],
            })
            .collect();
    }

    fn branches(&self, item: syn::ItemUse) -> Vec<syn::ItemUse> {
        match &item.tree {
            UseTree::Group(group) if item.leading_colon.is_none() && self.starts(&item.tree) => {
                group
                    .items
                    .iter()
                    .flat_map(|tree| {
                        self.branches(syn::ItemUse {
                            tree: tree.clone(),
                            ..item.clone()
                        })
                    })
                    .collect()
            }
            _ => vec![item],
        }
    }

    /// Whether a branch of a `use` tree starts with another crate's
    /// `$crate`.
    fn starts(&self, tree: &UseTree) -> bool {
        match tree {
            UseTree::Path(path) => self.name(&path.ident).is_some(),
            UseTree::Name(name) => self.name(&name.ident).is_some(),
            UseTree::Rename(rename) => self.name(&rename.ident).is_some(),
            UseTree::Group(group) => group.items.iter().any(|tree| self.starts(tree)),
            UseTree::Glob(_) => false,
        }
    }

    /// The crate `ident` stands for, if it is another crate's `$crate`,
    /// written where it is.
    fn name(&self, ident: &Ident) -> Option<Ident> {
        if ident != "crate" {
            return None;
        }
        let span = ident.span();
        let start = span.start();
        let mut name = self
            .0
            .get(&(span.file(), start.line, start.column))?
            .clone();
        name.set_span(span);
        Some(name)
    }
}

impl VisitMut for DollarCrates<'_> {
    fn visit_item_mod_mut(&mut self, item: &mut ItemMod) {
        if let Some((_, items)) = &mut item.content {
            self.split(items);
        }
        visit_mut::visit_item_mod_mut(self, item);
    }

    fn visit_block_mut(&mut self, block: &mut Block) {
        block.stmts = mem::take(&mut block.stmts)
            .into_iter()
            .flat_map(|stmt| match stmt {
                Stmt::Item(Item::Use(item)) => self
                    .branches(item)
                    .into_iter()
                    .map(|item| Stmt::Item(Item::Use(item)))
                    .collect(),
                stmt => vec![stmt],
            })
            .collect();
        visit_mut::visit_block_mut(self, block);
    }

    fn visit_path_mut(&mut self, path: &mut syn::Path) {
        if path.leading_colon.is_none()
            && let Some(first) = path.segments.first_mut()
            && let Some(name) = self.name(&first.ident)
        {
            path.leading_colon = Some(Token![::](name.span()));
            first.ident = name;
        }
        visit_mut::visit_path_mut(self, path);
    }

    fn visit_item_use_mut(&mut self, item: &mut syn::ItemUse) {
        if item.leading_colon.is_none() {
            let first = match &mut item.tree {
                UseTree::Path(path) => Some(&mut path.ident),
                UseTree::Name(name) => Some(&mut name.ident),
                UseTree::Rename(rename) => Some(&mut rename.ident),
                UseTree::Glob(_) | UseTree::Group(_) => None,
            };
            if let Some(first) = first
                && let Some(name) = self.name(first)
            {
                item.leading_colon = Some(Token![::](name.span()));
                *first = name;
            }
        }
        visit_mut::visit_item_use_mut(self, item);
    }
}

/// Replaces each item macro whose body parses as items with those items, as
/// macros such as Tokio's `cfg_rt! { ... }` expand to their input, and each
/// `cfg_if!` or `cfg_select!` with the items of its branches, under the
/// `cfg` it gives them.
/// The macro's own attributes carry over to each item, so a `cfg` on it still
/// applies. A call with an empty body, such as `crate_root!();`, is left
/// for [`Scanner::index`] to expand, since it can only be read through the
/// macro's definition.
struct Expand;

impl Expand {
    /// Expands the items of a module, as in a file, and those of the modules
    /// and functions among them.
    fn fragment(&mut self, items: &mut Vec<Item>) {
        self.items(items);
        for item in items {
            self.visit_item_mut(item);
        }
    }

    fn items(&mut self, items: &mut Vec<Item>) {
        let mut expanded = Vec::with_capacity(items.len());
        for item in mem::take(items) {
            let Item::Macro(item) = item else {
                expanded.push(item);
                continue;
            };
            if item.ident.is_some() || is_include(&item.mac) || item.mac.tokens.is_empty() {
                expanded.push(Item::Macro(item));
                continue;
            }
            let parsed = if is_cfg_if(&item.mac) {
                item.mac.parse_body_with(parse_cfg_if)
            } else if is_cfg_select(&item.mac) {
                item.mac.parse_body_with(parse_cfg_select)
            } else {
                item.mac.parse_body_with(parse_items)
            };
            match parsed {
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

/// The attributes as they apply after `cfg_attr` is expanded, each with the
/// predicates, as written, of the `cfg_attr`s it is in. A `cfg_attr` that
/// doesn't parse adds none.
fn conditional(attrs: &[Attribute]) -> Vec<(Vec<TokenStream>, Meta)> {
    let mut metas = Vec::new();
    for attr in attrs {
        flatten(&mut Vec::new(), attr.meta.clone(), &mut metas);
    }
    metas
}

fn flatten(
    conditions: &mut Vec<TokenStream>,
    meta: Meta,
    metas: &mut Vec<(Vec<TokenStream>, Meta)>,
) {
    let Meta::List(list) = &meta else {
        metas.push((conditions.clone(), meta));
        return;
    };
    if !list.path.is_ident("cfg_attr") {
        metas.push((conditions.clone(), meta));
        return;
    }
    let parsed = list.parse_args_with(|input: ParseStream| {
        let mut condition = TokenStream::new();
        while !input.peek(Token![,]) {
            condition.extend([input.parse::<TokenTree>()?]);
        }
        input.parse::<Token![,]>()?;
        let inner = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        Ok((condition, inner))
    });
    let Ok((condition, inner)) = parsed else {
        return;
    };
    conditions.push(condition);
    for meta in inner {
        flatten(conditions, meta, metas);
    }
    conditions.pop();
}

/// The predicates of the `cfg`s that attributes put on code, as written. A
/// `cfg` that `#[cfg_attr(q, cfg(p))]` adds is `any(not(all(q)), p)`.
fn cfg_predicates(attrs: &[Attribute]) -> Vec<TokenStream> {
    let span = Span::call_site();
    conditional(attrs)
        .into_iter()
        .filter_map(|(conditions, meta)| {
            let Meta::List(list) = meta else {
                return None;
            };
            if !list.path.is_ident("cfg") {
                return None;
            }
            if conditions.is_empty() {
                return Some(list.tokens);
            }
            let unless = cfg_call("not", [cfg_call("all", conditions, span)], span);
            Some(cfg_call("any", [unless, list.tokens], span))
        })
        .collect()
}

fn parse_items(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut items = Vec::new();
    while !input.is_empty() {
        items.push(input.parse()?);
    }
    Ok(items)
}

/// Parses the body of a `cfg_if!`, `if #[cfg(a)] { ... } else if #[cfg(b)]
/// { ... } else { ... }`, into the items of its branches, as the `cfg_if`
/// crate expands it: each branch's items get
/// `#[cfg(all(<own>, not(any(<earlier>, ...))))]`, and the `else` branch's
/// `#[cfg(not(any(<earlier>, ...)))]`.
fn parse_cfg_if(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut earlier = Vec::new();
    let mut items = Vec::new();
    let mut otherwise = None;
    loop {
        let (span, predicate) = match otherwise {
            None => {
                input.parse::<Token![if]>()?;
                let (span, condition) = cfg_if_condition(input)?;
                let none_earlier = cfg_call("not", [cfg_call("any", earlier.clone(), span)], span);
                earlier.push(condition.clone());
                (span, cfg_call("all", [condition, none_earlier], span))
            }
            Some(span) => (
                span,
                cfg_call("not", [cfg_call("any", earlier.clone(), span)], span),
            ),
        };
        let body;
        braced!(body in input);
        let cfg = cfg_attribute(predicate, span);
        for mut item in parse_items(&body)? {
            if let Some(attrs) = item_attrs_mut(&mut item) {
                let own = mem::take(attrs);
                attrs.push(cfg.clone());
                attrs.extend(own);
            }
            items.push(item);
        }
        if otherwise.is_some() || input.is_empty() {
            return Ok(items);
        }
        let span = input.parse::<Token![else]>()?.span;
        if !input.peek(Token![if]) {
            otherwise = Some(span);
        }
    }
}

/// What a `cfg_if!` branch's `#[cfg(...)]` holds, as written, and where the
/// attribute is written. The `cfg_if` crate splices it into other
/// predicates as it is, so `#[cfg(a, b)]` is `all(a, b, ...)` for its own
/// branch and `any(a, b, ...)` for the branches after it.
fn cfg_if_condition(input: ParseStream) -> syn::Result<(Span, TokenStream)> {
    let attrs = input.call(Attribute::parse_outer)?;
    let [attr] = &attrs[..] else {
        return Err(input.error("expected one #[cfg(...)]"));
    };
    match &attr.meta {
        Meta::List(list) if list.path.is_ident("cfg") && !list.tokens.is_empty() => {
            let span = attr.pound_token.span;
            Ok((span, list.tokens.clone()))
        }
        _ => Err(input.error("expected #[cfg(...)]")),
    }
}

/// Parses item-position `cfg_select!` arms, giving each item the predicate
/// that selects its arm and excludes every earlier arm. The trailing `_`
/// arm only excludes earlier predicates.
fn parse_cfg_select(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut earlier = Vec::new();
    let mut items = Vec::new();
    loop {
        let span = input.span();
        let otherwise = input.peek(Token![_]);
        let none_earlier = cfg_call("not", [cfg_call("any", earlier.clone(), span)], span);
        let predicate = if otherwise {
            input.parse::<Token![_]>()?;
            none_earlier
        } else {
            let mut condition = TokenStream::new();
            while !input.peek(Token![=>]) {
                condition.extend([input.parse::<TokenTree>()?]);
            }
            if condition.is_empty() {
                return Err(input.error("expected a cfg predicate"));
            }
            earlier.push(condition.clone());
            cfg_call("all", [condition, none_earlier], span)
        };
        input.parse::<Token![=>]>()?;
        let body;
        braced!(body in input);
        let cfg = cfg_attribute(predicate, span);
        for mut item in parse_items(&body)? {
            if let Some(attrs) = item_attrs_mut(&mut item) {
                let own = mem::take(attrs);
                attrs.push(cfg.clone());
                attrs.extend(own);
            }
            items.push(item);
        }
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if otherwise || input.is_empty() {
            return Ok(items);
        }
    }
}

/// The predicate `name(args, ...)`.
fn cfg_call(name: &str, args: impl IntoIterator<Item = TokenStream>, span: Span) -> TokenStream {
    let mut list = TokenStream::new();
    for arg in args {
        if !list.is_empty() {
            list.extend([TokenTree::Punct(Punct::new(',', Spacing::Alone))]);
        }
        list.extend(arg);
    }
    let mut group = Group::new(Delimiter::Parenthesis, list);
    group.set_span(span);
    TokenStream::from_iter([
        TokenTree::Ident(Ident::new(name, span)),
        TokenTree::Group(group),
    ])
}

/// The attribute `#[cfg(predicate)]`, written at `span`. Attributes are
/// told apart by where they are written, so `span` must be a place no other
/// attribute starts at.
fn cfg_attribute(predicate: TokenStream, span: Span) -> Attribute {
    Attribute {
        pound_token: Token![#](span),
        style: AttrStyle::Outer,
        bracket_token: token::Bracket(span),
        meta: Meta::List(MetaList {
            path: Ident::new("cfg", span).into(),
            delimiter: MacroDelimiter::Paren(token::Paren(span)),
            tokens: predicate,
        }),
    }
}

/// Whether a macro is `cfg_if!`, by its name. Crates such as `libc` define
/// their own copy rather than depend on the `cfg_if` crate, so the name is
/// what they share. A body that isn't `cfg_if!`'s syntax stays a macro.
fn is_cfg_if(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "cfg_if")
}

fn is_cfg_select(mac: &Macro) -> bool {
    let mut names = mac.path.segments.iter().map(|segment| &segment.ident);
    match (names.next(), names.next(), names.next()) {
        (Some(name), None, None) => name == "cfg_select",
        (Some(root), Some(name), None) => (root == "std" || root == "core") && name == "cfg_select",
        _ => false,
    }
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
    /// Where the attributes already visited are written.
    attributes: HashSet<Site>,
}

impl FileVisitor<'_, '_> {
    fn enabled(&self, attrs: &[Attribute]) -> bool {
        self.scanner.krate.cfg.enabled(attrs) != Truth::False
    }

    /// Where the token at `span` is written: in the file being visited,
    /// or, in a macro's expansion, in the call's file or the macro's.
    fn site(&self, span: Span) -> Site {
        let start = span.start();
        Site {
            file: self.scanner.file_of(span, &self.file),
            line: start.line,
            column: start.column + 1,
        }
    }

    /// Whether the token at `span` is written in this crate, not in another
    /// crate's macro that it calls. Code counts for the crate that writes
    /// it, as rustc's `unsafe_code` lint leaves out what another crate's
    /// macro writes.
    fn here(&self, span: Span) -> bool {
        !self.scanner.foreign.contains(&span.file())
    }

    /// Whether `unsafe` is written here, if it is written at all.
    fn writes(&self, unsafety: Option<&Token![unsafe]>) -> bool {
        unsafety.is_some_and(|token| self.here(token.span))
    }

    /// Whether an item is `unsafe` here.
    fn writes_unsafe(&self, safety: &Safety) -> bool {
        match safety {
            Safety::Unsafe(token) => self.writes(Some(token)),
            _ => false,
        }
    }

    fn unscanned(&mut self, span: Span, reason: String) {
        let site = self.site(span);
        self.scanner.unscanned(Unscanned {
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

    /// Notes a call of a macro that isn't read as its expansion.
    fn unexpanded(&mut self, mac: &Macro) {
        if let Some(segment) = mac.path.segments.last() {
            self.scanner
                .unexpanded
                .insert(segment.ident.unraw().to_string());
        }
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
        let mut resolver = Resolver::new(self.scanner, &self.blocks);
        let names = segments(&expr.path);
        let leading = expr.path.leading_colon.is_some();
        let resolved = match &expr.qself {
            // `<[T]>::split_at` and `<Vec<T>>::insert` name the type's items.
            Some(qself) if qself.position == 0 => {
                let base = match &*qself.ty {
                    Type::Slice(_) => Resolved::Path(vec!["slice".to_owned()]),
                    Type::Path(ty) if ty.qself.is_none() => resolver.path(
                        &self.module,
                        Namespace::Types,
                        &segments(&ty.path),
                        ty.path.leading_colon.is_some(),
                    ),
                    _ => Resolved::Unknown,
                };
                base.join(&names)
            }
            // `<T as Trait>::method` names the trait's item.
            _ => resolver.path(&self.module, Namespace::Values, &names, leading),
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
    /// and calls to a name some disallowed path has. A `mod name;` there
    /// names a file the scan doesn't load, so it is listed as unscanned. The
    /// macros called there aren't expanded.
    fn tokens(&mut self, stream: TokenStream) {
        let mut previous: Option<TokenTree> = None;
        let mut before: Option<TokenTree> = None;
        for tree in stream {
            match &tree {
                TokenTree::Ident(ident) if ident == "unsafe" && self.here(ident.span()) => {
                    self.scanner.scan.unsafe_code.in_macros += 1;
                }
                TokenTree::Punct(punct) if punct.as_char() == ';' => {
                    if let (Some(TokenTree::Ident(name)), Some(TokenTree::Ident(keyword))) =
                        (&previous, &before)
                        && keyword == "mod"
                    {
                        self.unscanned(
                            name.span(),
                            format!(
                                "module `{}` is declared in a macro body the scan can't read",
                                name.unraw()
                            ),
                        );
                    }
                }
                TokenTree::Group(group) => {
                    if let (Some(TokenTree::Punct(bang)), Some(TokenTree::Ident(name))) =
                        (&previous, &before)
                        && bang.as_char() == '!'
                        && name != "macro_rules"
                    {
                        self.scanner.unexpanded.insert(name.unraw().to_string());
                    }
                    if group.delimiter() == Delimiter::Bracket
                        && is_attribute(previous.as_ref(), before.as_ref())
                        && self.here(group.span())
                    {
                        self.scanner.scan.unsafe_code.in_macros +=
                            self.bare_unsafe_attributes(group.stream());
                    }
                    if group.delimiter() == Delimiter::Parenthesis
                        && let Some(TokenTree::Ident(name)) = &previous
                        && self.here(name.span())
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

    /// The attributes that `unsafe_code` reports, written without
    /// `unsafe(...)`, among those an attribute in a macro body stands for
    /// once `cfg_attr` is expanded. One written as `unsafe(...)` counts as
    /// the `unsafe` in it. The attribute is read as tokens, not parsed, since
    /// its value may be a metavariable, as in `#[export_name = $name]`, and a
    /// `cfg_attr` predicate that doesn't parse is unknown.
    fn bare_unsafe_attributes(&self, attribute: TokenStream) -> usize {
        let mut trees = attribute.into_iter();
        let Some(TokenTree::Ident(name)) = trees.next() else {
            return 0;
        };
        if UNSAFE_ATTRIBUTES
            .iter()
            .any(|unsafe_name| name == unsafe_name)
        {
            return 1;
        }
        if name != "cfg_attr" {
            return 0;
        }
        let (Some(TokenTree::Group(group)), None) = (trees.next(), trees.next()) else {
            return 0;
        };
        let mut parts = split_commas(group.stream()).into_iter();
        let Some(predicate) = parts.next() else {
            return 0;
        };
        if self.scanner.krate.cfg.holds(predicate) == Truth::False {
            return 0;
        }
        parts.map(|part| self.bare_unsafe_attributes(part)).sum()
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
            (None, Location::Files(files)) => {
                // The scan doesn't keep the macros in textual scope inside
                // functions, so none of its calls are expanded.
                let textual = &mut Textual::default();
                for file in files {
                    let declared = self.site(item.ident.span()).file;
                    let from = (declared.as_str(), item.ident.span());
                    self.scanner
                        .load(&file.path, module.clone(), file.mod_rs, Some(from), textual);
                }
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
        let mut scope = Scope::default();
        self.scanner.bind(&mut scope, items, None, false);
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

/// Whether a bracketed group that follows these tokens is an attribute:
/// `#[...]` or `#![...]`.
fn is_attribute(previous: Option<&TokenTree>, before: Option<&TokenTree>) -> bool {
    is_punct(previous, '#') || (is_punct(previous, '!') && is_punct(before, '#'))
}

/// The comma-separated parts of a token stream, without empty ones, as a
/// trailing comma leaves.
fn split_commas(stream: TokenStream) -> Vec<TokenStream> {
    let mut parts = vec![TokenStream::new()];
    for tree in stream {
        if is_punct(Some(&tree), ',') {
            parts.push(TokenStream::new());
        } else if let Some(part) = parts.last_mut() {
            part.extend([tree]);
        }
    }
    parts.retain(|part| !part.is_empty());
    parts
}

fn is_punct(tree: Option<&TokenTree>, char: char) -> bool {
    matches!(tree, Some(TokenTree::Punct(punct)) if punct.as_char() == char)
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
        match &item.ident {
            // A `macro_rules!` that was expanded is read where it is called,
            // and as tokens only if some call of it isn't expanded.
            Some(name) => {
                let site = self.site(name.span());
                let at = (site.file, site.line, name.span().start().column);
                if self.scanner.expanded.contains(&at) {
                    let deferred = Deferred {
                        name: name.unraw().to_string(),
                        module: self.module.clone(),
                        mod_dir: self.mod_dir.clone(),
                        path_dir: self.path_dir.clone(),
                        tokens: item.mac.tokens.clone(),
                    };
                    self.scanner.deferred.insert(at, deferred);
                    return;
                }
            }
            // The calls that are expanded aren't among the items.
            None => self.unexpanded(&item.mac),
        }
        // A `macro_rules!` definition, or an invocation whose body isn't
        // items.
        self.tokens(item.mac.tokens.clone());
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if self.writes_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_item_fn(this, item);
            });
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if self.writes(item.unsafety.as_ref()) {
            self.scanner.scan.unsafe_code.impls += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_impl(this, item));
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if self.writes(item.unsafety.as_ref()) {
            self.scanner.scan.unsafe_code.traits += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_trait(this, item));
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast ItemForeignMod) {
        if self.writes(item.unsafety.as_ref()) {
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
        if self.writes_unsafe(&item.sig.safety) {
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
        if self.writes_unsafe(&item.sig.safety) {
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
        if self.here(attr.pound_token.span)
            && self.attributes.insert(self.site(attr.pound_token.span))
        {
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
        if self.here(expr.unsafe_token.span) {
            self.scanner.scan.unsafe_code.blocks += 1;
        }
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
        let start = Self::start(expr);
        if self.here(start) {
            for entry in entries {
                let site = self.site(start);
                self.scanner.scan.calls.entry(entry).or_default().push(site);
            }
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
            && self.here(Self::start(func))
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
        if self.scanner.disallowed.has_method(&name) && self.here(expr.method.span()) {
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
        self.unexpanded(mac);
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

    use super::{
        super::cfg::{Features, Target},
        *,
    };

    /// Each `cfg_if!` branch's items are built only where its predicate
    /// holds and no earlier one does, as the `cfg_if` crate expands it.
    #[test]
    fn cfg_if_gives_each_branch_the_cfg_that_selects_it() {
        let target = Target::parse("unix\ntarget_os=\"linux\"\n");
        let features = Features::default();
        let cfg = Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        };
        let mut file = syn::parse_file(
            r#"
            cfg_if::cfg_if! {
                if #[cfg(windows)] { fn a() {} }
                else if #[cfg(unix)] { fn b() {} struct B; }
                else if #[cfg(loom)] { fn c() {} }
                else { fn d() {} }
            }
            cfg_if! {
                if #[cfg(loom)] {
                    fn e() {}
                    cfg_if! {
                        if #[cfg(unix)] { fn f() {} } else { fn g() {} }
                    }
                } else {
                    #[cfg(windows)]
                    fn h() {}
                    fn i() {}
                }
            }
            cfg_if! { if #[cfg(unix, windows)] { fn j() {} } else { fn l() {} } }
            #[cfg(windows)]
            cfg_if! { if #[cfg(unix)] { fn k() {} } }
            "#,
        )
        .unwrap();
        Expand.visit_file_mut(&mut file);
        let built: Vec<(String, Truth)> = file
            .items
            .iter()
            .map(|item| {
                let name = match item {
                    Item::Fn(item) => item.sig.ident.to_string(),
                    Item::Struct(item) => item.ident.to_string(),
                    _ => panic!("an item that isn't a function or struct"),
                };
                (name, cfg.enabled(item_attrs(item)))
            })
            .collect();
        let expected = [
            ("a", Truth::False),
            ("b", Truth::True),
            ("B", Truth::True),
            ("c", Truth::False),
            ("d", Truth::False),
            ("e", Truth::Unknown),
            ("f", Truth::Unknown),
            ("g", Truth::False),
            ("h", Truth::False),
            ("i", Truth::Unknown),
            ("j", Truth::False),
            ("l", Truth::False),
            ("k", Truth::False),
        ]
        .map(|(name, truth)| (name.to_owned(), truth));
        assert_eq!(built, expected);
    }

    /// A body that isn't `cfg_if!`'s syntax, or one with that syntax in a
    /// macro of another name, is left as a macro.
    #[test]
    fn only_cfg_if_bodies_are_read_as_branches() {
        for source in [
            "cfg_if! { if #[cfg(unix)] { fn a() {} } else }",
            "cfg_if! { if #[cfg(unix)] #[cfg(windows)] { fn a() {} } }",
            "cfg_if! { if #[cfg] { fn a() {} } }",
            "cfg_if! { if #[cfg()] { fn a() {} } }",
            "cfg_if! { if #[cfg_attr(unix, cfg(windows))] { fn a() {} } }",
            "cfg_if! { if #[cfg(unix)] { fn a() {} } else { fn b() {} } else { fn c() {} } }",
            "cfg_if! { if #[cfg(unix)] { mod a; 1 + 1 } }",
            "other! { if #[cfg(unix)] { fn a() {} } }",
        ] {
            let mut file = syn::parse_file(source).unwrap();
            Expand.visit_file_mut(&mut file);
            assert!(
                matches!(&file.items[..], [Item::Macro(_)]),
                "{source}: {:?}",
                file.items.len()
            );
        }
    }

    #[test]
    fn cfg_select_gives_each_arm_the_cfg_that_selects_it() {
        let target = Target::parse("unix\ntarget_pointer_width=\"64\"\n");
        let features = Features::default();
        let cfg = Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        };
        let mut file = syn::parse_file(
            r#"
            cfg_select! {
                windows => { fn a() {} },
                all(unix, target_pointer_width = "64") => { fn b() {} struct B; }
                platform_custom => { fn c() {} }
                _ => { fn d() {} },
            }
            std::cfg_select! {
                platform_custom => {
                    fn e() {}
                    core::cfg_select! {
                        unix => { fn f() {} }
                        _ => { fn g() {} }
                    }
                }
                _ => {
                    #[cfg(windows)]
                    fn h() {}
                    fn i() {}
                }
            }
            ::core::cfg_select! {
                target_pointer_width = "8" => { fn j() {} }
                _ => { fn k() {} }
            }
            #[cfg(windows)]
            ::std::cfg_select! { unix => { fn l() {} } }
            cfg_select! { _ => { fn m() {} } }
            cfg_select! { feature = "off" => { fn n() {} } }
            cfg_select! {
                platform_custom => { fn o() {} }
                unix => { fn p() {} }
                platform_later => { fn q() {} }
                _ => { fn r() {} }
            }
            "#,
        )
        .unwrap();
        Expand.visit_file_mut(&mut file);
        let built: Vec<_> = file
            .items
            .iter()
            .map(|item| {
                let name = match item {
                    Item::Fn(item) => item.sig.ident.to_string(),
                    Item::Struct(item) => item.ident.to_string(),
                    _ => panic!("an item that isn't a function or struct"),
                };
                (name, cfg.enabled(item_attrs(item)))
            })
            .collect();
        let expected = [
            ("a", Truth::False),
            ("b", Truth::True),
            ("B", Truth::True),
            ("c", Truth::False),
            ("d", Truth::False),
            ("e", Truth::Unknown),
            ("f", Truth::Unknown),
            ("g", Truth::False),
            ("h", Truth::False),
            ("i", Truth::Unknown),
            ("j", Truth::False),
            ("k", Truth::True),
            ("l", Truth::False),
            ("m", Truth::True),
            ("n", Truth::False),
            ("o", Truth::Unknown),
            ("p", Truth::Unknown),
            ("q", Truth::False),
            ("r", Truth::False),
        ]
        .map(|(name, truth)| (name.to_owned(), truth));
        assert_eq!(built, expected);
    }

    #[test]
    fn unreadable_cfg_select_bodies_stay_macros() {
        for source in [
            "cfg_select! {}",
            "cfg_select! { => { fn a() {} } }",
            "cfg_select! { unix { fn a() {} } }",
            "cfg_select! { unix => fn a() {} }",
            "cfg_select! { unix => { mod a; 1 + 1 } }",
            "cfg_select! { _ => { fn a() {} } unix => { fn b() {} } }",
            "cfg_select! { unix => { fn a() {} },, }",
            "cfg_select! { unix => { fn a() {} } _ => 42 }",
            "other! { unix => { fn a() {} } }",
            "other::cfg_select! { unix => { fn a() {} } }",
            "std::other::cfg_select! { unix => { fn a() {} } }",
        ] {
            let mut file = syn::parse_file(source).unwrap();
            Expand.visit_file_mut(&mut file);
            assert!(
                matches!(&file.items[..], [Item::Macro(_)]),
                "{source}: {:?}",
                file.items.len()
            );
        }
    }

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

    /// Scans a crate named `demo`, built for Linux with no features, whose
    /// files under `src/` are `files`.
    fn scan_files(files: &[(&str, &str)]) -> Scan {
        scan_with(files, false, &[])
    }

    /// A library a test crate depends on.
    #[derive(Clone, Copy)]
    struct Library<'t> {
        /// Its crate name, which the crate that depends on it calls it by.
        name: &'t str,
        /// The features its build has.
        features: &'t [&'t str],
        /// Its files under `src/`.
        files: &'t [(&'t str, &'t str)],
    }

    /// A new directory with `files` under `src/`.
    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join("src").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    /// Scans a crate named `demo`, built for Linux with no features, whose
    /// files under `src/` are `files`, in the 2015 edition if
    /// `edition_2015`, and which depends on `libraries`.
    fn scan_with(files: &[(&str, &str)], edition_2015: bool, libraries: &[Library]) -> Scan {
        let target = Target::parse("unix\ntarget_os=\"linux\"\n");
        let no_externs = BTreeMap::new();
        let no_dependencies = BTreeMap::new();
        let dirs: Vec<_> = libraries
            .iter()
            .map(|library| package(library.files))
            .collect();
        let srcs: Vec<_> = dirs
            .iter()
            .map(|dir| dir.path().join("src/lib.rs"))
            .collect();
        let features: Vec<_> = libraries
            .iter()
            .map(|library| {
                Features::from_sets(&[library.features.iter().map(|f| (*f).to_owned()).collect()])
            })
            .collect();
        let crates: Vec<_> = libraries
            .iter()
            .zip(&dirs)
            .zip(&srcs)
            .zip(&features)
            .map(|(((library, dir), src), features)| Crate {
                name: library.name,
                root: dir.path(),
                src,
                edition_2015: false,
                externs: &no_externs,
                dependencies: &no_dependencies,
                cfg: Cfg {
                    target: &target,
                    features,
                    proc_macro: false,
                },
            })
            .collect();
        let externs = libraries
            .iter()
            .map(|library| (library.name.to_owned(), library.name.to_owned()))
            .collect();
        let dependencies = libraries
            .iter()
            .zip(&crates)
            .map(|(library, krate)| (library.name.to_owned(), krate))
            .collect();
        let dir = package(files);
        let features = Features::default();
        let src = dir.path().join("src/lib.rs");
        let krate = Crate {
            name: "demo",
            root: dir.path(),
            src: &src,
            edition_2015,
            externs: &externs,
            dependencies: &dependencies,
            cfg: Cfg {
                target: &target,
                features: &features,
                proc_macro: false,
            },
        };
        scan(&krate, &Disallowed::bundled().unwrap())
    }

    fn unscanned(file: &str, line: usize, reason: &str) -> Unscanned {
        Unscanned {
            file: file.to_owned(),
            line: Some(line),
            reason: reason.to_owned(),
        }
    }

    /// A call of the crate's own `macro_rules!` with one rule and an empty
    /// matcher, as serde's `crate_root!();`, is read as its expansion: its
    /// modules are the calling module's, its code counts once, and its sites
    /// are in the macro's file.
    #[test]
    fn calls_of_crate_macros_without_input_are_expanded() {
        let scan = scan_files(&[
            ("lib.rs", "#[macro_use]\nmod macros;\n\nroot!();\n"),
            (
                "macros.rs",
                r"macro_rules! root {
    () => {
        mod engine;
        pub struct Handle;
        unsafe impl Send for Handle {}
        pub fn start() {
            std::thread::spawn(|| ());
        }
    };
}
",
            ),
            ("engine.rs", "pub unsafe fn run() {}\n"),
        ]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(
            scan.unsafe_code,
            UnsafeCounts {
                functions: 1,
                impls: 1,
                ..UnsafeCounts::default()
            }
        );
        let sites: Vec<_> = scan.calls.values().flatten().collect();
        let site = Site {
            file: "src/macros.rs".to_owned(),
            line: 7,
            column: 13,
        };
        assert_eq!(sites, [&site]);
    }

    /// A module's file, or an inline module, can make it `#[macro_use]`
    /// with an inner attribute.
    #[test]
    fn inner_macro_use_keeps_a_modules_macros_in_scope() {
        let scan = scan_files(&[
            (
                "lib.rs",
                r"macro_rules! root { () => { mod old; }; }
mod macros;
root!();
mod inline {
    #![macro_use]
    macro_rules! other { () => { mod inner; }; }
}
other!();
",
            ),
            (
                "macros.rs",
                "#![macro_use]\nmacro_rules! root { () => { mod new; }; }\n",
            ),
            ("new.rs", "pub unsafe fn a() {}\n"),
            ("inner.rs", "pub unsafe fn b() {}\n"),
        ]);
        let reason = "module `old` is declared in a macro body the scan can't read";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 1, reason)]);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    /// A macro some of whose calls aren't expanded, as in a function, is
    /// read as tokens too, so what those calls may reach still counts.
    #[test]
    fn macros_with_calls_that_are_not_expanded_are_read_as_tokens() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! runner {
    () => {
        pub fn run() {
            fn spawn(_: fn()) {}
            spawn(|| ());
        }
    };
}
runner!();
pub fn outer() {
    use std::thread::spawn;
    runner!();
}
",
        )]);
        assert_eq!(scan.unresolved, BTreeMap::from([("spawn".to_owned(), 1)]));
        assert_eq!(scan.calls, BTreeMap::new());
    }

    /// A call expands only a macro in textual scope: one defined before it
    /// in its module or a module around it, or in a `#[macro_use]` module
    /// declared before it. A macro no call expands is read as tokens.
    #[test]
    fn only_macros_in_textual_scope_are_expanded() {
        let scan = scan_files(&[
            (
                "lib.rs",
                r"mod private {
    macro_rules! private { () => { mod one; }; }
}
private!();
macro_rules! early { () => { mod two; }; }
mod outer {
    early!();
}
",
            ),
            ("outer/two.rs", "pub unsafe fn f() {}\n"),
            ("one.rs", "pub unsafe fn g() {}\n"),
        ]);
        let reason = "module `one` is declared in a macro body the scan can't read";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 2, reason)]);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    /// Each definition a call may name expands under the `cfg`s that select
    /// it, and one every build has hides the ones before it. Those it hides,
    /// and those out of scope, are read as tokens.
    #[test]
    fn calls_expand_each_definition_a_build_may_have() {
        let scan = scan_files(&[
            (
                "lib.rs",
                r"macro_rules! pick { () => { mod hidden; }; }
macro_rules! pick { () => { mod first; }; }
#[cfg(demo_custom)]
macro_rules! pick { () => { mod custom; }; }
#[cfg(windows)]
macro_rules! pick { () => { mod never; }; }
#[cfg_attr(windows, macro_use)]
mod nothing {
    macro_rules! pick { () => { mod never; }; }
}
pick!();
",
            ),
            ("first.rs", "pub unsafe fn f() {}\n"),
            ("custom.rs", "pub unsafe fn g() {}\n"),
        ]);
        let reason =
            |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
        let listed = [(1, "hidden"), (9, "never")]
            .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
        assert_eq!(scan.unscanned, listed);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    /// A call of a macro that a `#[macro_use] extern crate` brings in, as
    /// stacker's `psm_stack_manipulation!`, is read as each definition the
    /// other crate's build may have: psm defines the macro twice, under a
    /// `cfg` its build script sets, so both arms are scanned, and the names
    /// each binds may be either. The macros are the whole crate's, before
    /// the item too.
    #[test]
    fn calls_of_macros_another_crate_exports_are_expanded() {
        let psm = Library {
            name: "psm",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[cfg(switchable_stack)]
#[macro_export]
macro_rules! psm_stack_manipulation {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => { $($yes)* };
}

#[cfg(not(switchable_stack))]
#[macro_export]
macro_rules! psm_stack_manipulation {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => { $($no)* };
}
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    r"psm_stack_manipulation! {
    yes {
        mod guard;
        use std::thread::spawn as go;
    }
    no {
        mod fallback;
        fn go(_: fn()) {}
    }
}

#[macro_use]
extern crate psm;

pub fn run() {
    go(|| ());
}
",
                ),
                ("guard.rs", "pub unsafe fn a() {}\n"),
                ("fallback.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[psm],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
        let sites: Vec<_> = scan.calls.values().flatten().collect();
        let site = Site {
            file: "src/lib.rs".to_owned(),
            line: 16,
            column: 5,
        };
        assert_eq!(sites, [&site]);
    }

    /// Which of another crate's definitions a call may name is decided by
    /// the `cfg`s of that crate's build, not of the calling crate's.
    #[test]
    fn other_crates_macros_are_defined_by_their_builds() {
        let fast = Library {
            name: "fast",
            features: &["fast"],
            files: &[(
                "lib.rs",
                r#"#[cfg(feature = "fast")]
#[macro_export]
macro_rules! pick { () => { mod quick; }; }

#[cfg(not(feature = "fast"))]
#[macro_export]
macro_rules! pick { () => { mod slow; }; }

#[cfg(windows)]
mod windows {
    #[macro_export]
    macro_rules! never { () => { mod never; }; }
}
"#,
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate fast;\n\npick!();\nnever!();\n",
                ),
                ("quick.rs", "pub unsafe fn a() {}\n"),
                ("slow.rs", "pub unsafe fn b() {}\n"),
                ("never.rs", "pub unsafe fn c() {}\n"),
            ],
            false,
            &[fast],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    /// `$crate` in another crate's macro names that crate. What the macro
    /// writes itself counts for that crate, as rustc's `unsafe_code` lint
    /// leaves it out; what the call passes it counts here.
    #[test]
    fn other_crates_macros_name_and_count_for_their_crate() {
        let tokio = Library {
            name: "tokio",
            features: &[],
            files: &[(
                "lib.rs",
                r"pub fn spawn() {}

#[macro_export]
macro_rules! runtime {
    ($name:ident { $($body:tt)* }) => {
        pub use $crate::spawn as start;
        pub unsafe fn hidden() {
            unsafe { std::thread::spawn(|| ()) };
        }
        unsafe impl Send for Handle {}
        pub fn $name() { $($body)* }
    };
}
",
            )],
        };
        for (edition_2015, item) in [
            (false, "extern crate tokio as rt;"),
            (true, "extern crate tokio as rt;"),
            (false, "extern crate tokio;"),
        ] {
            let lib = format!(
                "#[macro_use]\n{item}\n\nruntime! {{ mine {{ unsafe {{ std::hint::spin_loop() }} }} }}\n\npub struct Handle;\n\npub fn run() {{\n    start();\n}}\n"
            );
            let scan = scan_with(&[("lib.rs", &lib)], edition_2015, slice::from_ref(&tokio));
            assert_eq!(scan.unscanned, [], "{item}");
            assert_eq!(
                scan.unsafe_code,
                UnsafeCounts {
                    blocks: 1,
                    ..UnsafeCounts::default()
                },
                "{item}"
            );
            let calls: Vec<_> = scan
                .calls
                .iter()
                .map(|(entry, sites)| {
                    (
                        Disallowed::bundled().unwrap().entries[*entry].path.clone(),
                        sites.clone(),
                    )
                })
                .collect();
            let site = Site {
                file: "src/lib.rs".to_owned(),
                line: 9,
                column: 5,
            };
            assert_eq!(calls, [("tokio::spawn".to_owned(), vec![site])], "{item}");
        }
    }

    /// The prelude a `#[macro_use] extern crate` adds comes after textual
    /// scope. A `#[macro_use(...)]` brings in only the macros it names, and
    /// an `extern crate` without it none.
    #[test]
    fn other_crates_macros_come_after_textual_scope() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! pick { () => { mod theirs; }; }
#[macro_export]
macro_rules! skipped { () => { mod skipped; }; }
",
            )],
        };
        let plain = Library {
            name: "plain",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! plain { () => { mod plain; }; }\n",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    r"mod before;

macro_rules! pick { () => { mod own; }; }
pick!();
skipped!();
plain!();

#[macro_use(pick)]
extern crate picks;
extern crate plain;
",
                ),
                ("before.rs", "pick!();\n"),
                ("before/theirs.rs", "pub unsafe fn a() {}\n"),
                ("own.rs", "pub unsafe fn b() {}\n"),
                ("theirs.rs", "pub unsafe fn c() {}\n"),
                ("skipped.rs", "pub unsafe fn d() {}\n"),
                ("plain.rs", "pub unsafe fn e() {}\n"),
            ],
            false,
            &[picks, plain],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    /// A macro that a module's `use` items bring in, or its glob imports
    /// may, hides the prelude's, so a call there isn't expanded with it.
    #[test]
    fn other_crates_macros_are_hidden_by_imports() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { mod theirs; }; }\n",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate picks;\n\nmod open;\nmod imports;\nmod globs;\n",
                ),
                ("open.rs", "pick!();\n"),
                ("open/theirs.rs", "pub unsafe fn a() {}\n"),
                ("imports.rs", "use other::pick;\npick!();\n"),
                ("imports/theirs.rs", "pub unsafe fn b() {}\n"),
                ("globs.rs", "use other::*;\npick!();\n"),
                ("globs/theirs.rs", "pub unsafe fn c() {}\n"),
            ],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    /// Of two `#[macro_use] extern crate` items that bring in a macro of
    /// one name, the last hides the first, as in rustc, unless a build may
    /// not have it. A macro it hides isn't read, even where its rules
    /// can't be.
    #[test]
    fn later_macro_use_imports_hide_earlier_ones() {
        let unreadable = Library {
            name: "unreadable",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { $$ }; }\n",
            )],
        };
        let first = Library {
            name: "first",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { mod first; }; }\n",
            )],
        };
        let second = Library {
            name: "second",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { mod second; }; }\n",
            )],
        };
        for (cfg, functions) in [("", 1), ("#[cfg(demo_custom)]\n", 2)] {
            let lib = format!(
                "#[macro_use]\nextern crate first;\n{cfg}#[macro_use]\nextern crate second;\n\npick!();\n"
            );
            let scan = scan_with(
                &[
                    ("lib.rs", &lib),
                    ("first.rs", "pub unsafe fn a() {}\n"),
                    ("second.rs", "pub unsafe fn b() {}\n"),
                ],
                false,
                &[first, second],
            );
            assert_eq!(scan.unscanned, [], "{cfg}");
            assert_eq!(scan.unsafe_code.functions, functions, "{cfg}");
        }
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate unreadable;\n#[macro_use]\nextern crate second;\n\npick!();\n",
                ),
                ("second.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[unreadable, second],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    /// Macros that each of another crate's definitions of a macro defines
    /// may each be the one a build has, so one doesn't hide the other.
    #[test]
    fn macros_other_crates_macros_define_do_not_hide_each_other() {
        let configure = Library {
            name: "configure",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[cfg(choice)]
#[macro_export]
macro_rules! configure {
    () => { macro_rules! selected { () => { mod first; }; } };
}

#[cfg(not(choice))]
#[macro_export]
macro_rules! configure {
    () => { macro_rules! selected { () => { mod second; }; } };
}
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate configure;\n\nconfigure!();\nselected!();\n",
                ),
                ("first.rs", "pub unsafe fn a() {}\n"),
                ("second.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[configure],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    /// `$crate` passed on to another macro is one identifier, as in rustc,
    /// and still names its crate once the expansion is read, also in a
    /// `use` group. What the macro writes itself, as its own `begin()`,
    /// counts for its crate.
    #[test]
    fn crate_passed_on_names_its_crate() {
        let tokio = Library {
            name: "tokio",
            features: &[],
            files: &[(
                "lib.rs",
                r"pub fn spawn() {}

#[macro_export]
macro_rules! outer {
    () => { inner!($crate); };
}

#[macro_export]
macro_rules! inner {
    ($c:ident) => {
        mod target;
        pub use $c::spawn as go;
        pub use {$crate::spawn as start, std::thread::spawn as thread};
        pub fn local() { use {$crate::spawn as begin}; begin(); }
    };
    (::$c:ident) => { mod fallback; };
}
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate tokio;\n\nouter!();\n\npub fn run() {\n    go();\n    start();\n}\n",
                ),
                ("target.rs", "pub unsafe fn a() {}\n"),
                ("fallback.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[tokio],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
        let paths: Vec<_> = scan
            .calls
            .iter()
            .map(|(entry, sites)| {
                let path = Disallowed::bundled().unwrap().entries[*entry].path.clone();
                (path, sites.iter().map(|site| site.line).collect::<Vec<_>>())
            })
            .collect();
        assert_eq!(paths, [("tokio::spawn".to_owned(), vec![7, 8])]);
    }

    /// In a `#[macro_export(local_inner_macros)]` macro, a call by a bare
    /// name is a call of its crate's macro, by path, which isn't expanded,
    /// rather than of a macro of that name here.
    #[test]
    fn local_inner_macros_do_not_call_macros_here() {
        let outer = Library {
            name: "outer",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export(local_inner_macros)]
macro_rules! outer { () => { helper!(); }; }

#[macro_export]
macro_rules! helper { () => { mod theirs; }; }
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate outer;\n\nmacro_rules! helper { () => { mod mine; }; }\nouter!();\n",
                ),
                ("mine.rs", "pub unsafe fn a() {}\n"),
                ("theirs.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[outer],
        );
        // The crate's own `helper!` is called nowhere, so it is read as tokens.
        let reason = "module `mine` is declared in a macro body the scan can't read";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 4, reason)]);
        assert_eq!(scan.unsafe_code.functions, 0);
    }

    /// An exported macro that another of its crate's macros defines names
    /// that crate with the `$crate` the other one wrote.
    #[test]
    fn crate_in_generated_exports_names_its_crate() {
        let tokio = Library {
            name: "tokio",
            features: &[],
            files: &[(
                "lib.rs",
                r"pub fn spawn() {}

macro_rules! define {
    () => {
        #[macro_export]
        macro_rules! runtime { () => { pub use $crate::spawn as start; }; }
    };
}

define!();
",
            )],
        };
        let scan = scan_with(
            &[(
                "lib.rs",
                "#[macro_use]\nextern crate tokio;\n\nruntime!();\n\npub fn run() {\n    start();\n}\n",
            )],
            false,
            &[tokio],
        );
        assert_eq!(scan.unscanned, []);
        let paths: Vec<_> = scan
            .calls
            .iter()
            .map(|(entry, sites)| {
                let path = Disallowed::bundled().unwrap().entries[*entry].path.clone();
                (path, sites.iter().map(|site| site.line).collect::<Vec<_>>())
            })
            .collect();
        assert_eq!(paths, [("tokio::spawn".to_owned(), vec![7])]);
    }

    /// A call of the crate's own `macro_rules!` with input, as Tokio's
    /// `feature! { #![unix] mod symlink; }`, is read as the rule it matches:
    /// the modules it declares are scanned under the `cfg` the rule gives
    /// them, its code counts for each call, and its sites are where their
    /// tokens are written, in the call's file or the macro's.
    #[test]
    fn calls_of_crate_macros_with_input_are_expanded() {
        let scan = scan_files(&[
            (
                "lib.rs",
                r"#[macro_use]
mod macros;

feature! {
    #![unix]
    mod symlink;
    pub use self::symlink::symlink;
}

feature! {
    #![windows]
    mod windows;
}

feature! {
    #![demo_custom]
    mod missing;
}
",
            ),
            (
                "macros.rs",
                r"macro_rules! feature {
    (
        #![$meta:meta]
        $($item:item)*
    ) => {
        $(
            #[cfg($meta)]
            $item
        )*
        const _: fn() = || unsafe { std::thread::sleep(std::time::Duration::ZERO) };
    };
}
",
            ),
            (
                "symlink.rs",
                "pub unsafe fn symlink() {\n    std::thread::sleep(std::time::Duration::ZERO);\n}\n",
            ),
            ("windows.rs", "pub unsafe fn windows() {}\n"),
        ]);
        let missing = "module `missing` has no file: neither missing.rs nor missing/mod.rs";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 17, missing)]);
        assert_eq!(scan.unsafe_code.functions, 1);
        assert_eq!(scan.unsafe_code.blocks, 3);
        let site = |file: &str, line, column| Site {
            file: file.to_owned(),
            line,
            column,
        };
        let rule = site("src/macros.rs", 10, 37);
        let mut sites: Vec<_> = scan.calls.values().flatten().cloned().collect();
        sites.sort();
        assert_eq!(
            sites,
            [
                rule.clone(),
                rule.clone(),
                rule,
                site("src/symlink.rs", 2, 5)
            ]
        );
    }

    /// A call the scan can't match to a rule as rustc would is read as
    /// tokens, as is one that may name a macro whose rules it can't read or
    /// that a module that may be `#[macro_use]` defines.
    #[test]
    fn calls_the_scan_cannot_match_are_not_expanded() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! expr { ($e:expr) => { mod a; }; }
expr!(1 + 1);
macro_rules! unread { () => { ${ignore(x)} mod b; }; }
unread!();
macro_rules! nothing { (a) => { mod c; }; }
nothing!(b);
macro_rules! maybe { () => { mod d; }; }
#[cfg_attr(demo_custom, macro_use)]
mod inner {
    macro_rules! maybe { () => { mod e; }; }
}
maybe!();
",
        )]);
        let reason =
            |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
        let listed = [(1, "a"), (3, "b"), (5, "c"), (7, "d"), (10, "e")]
            .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
        assert_eq!(scan.unscanned, listed);
    }

    /// A macro that calls itself is followed only as deep as rustc follows
    /// it, and the call past that is read as tokens, with its input.
    #[test]
    fn expansion_stops_at_a_macro_that_calls_itself() {
        let scan = scan_files(&[(
            "lib.rs",
            "macro_rules! again { ($($t:tt)*) => { again!($($t)*); }; }\nagain!(unsafe {});\n",
        )]);
        let reason = "`again!` expands to more than 128 nested macro calls";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 1, reason)]);
        assert_eq!(scan.unsafe_code.in_macros, 1);
    }

    /// A macro whose expansion calls it more than once is followed only for
    /// so many calls in all, after its first calls reach the depth limit.
    /// Each call past a limit is listed once.
    #[test]
    fn expansion_stops_at_a_macro_that_calls_itself_twice() {
        let scan = scan_files(&[(
            "lib.rs",
            "macro_rules! fan { () => { fan!(); fan!(); }; }\nfan!();\n",
        )]);
        let deep = "`fan!` expands to more than 128 nested macro calls";
        let many = "the crate's macros expand to more than 10000 calls";
        assert_eq!(
            scan.unscanned,
            [
                unscanned("src/lib.rs", 1, deep),
                unscanned("src/lib.rs", 1, many)
            ]
        );
    }

    /// A module whose `#[path]` depends on an unknown `cfg` is read from
    /// each place a build may take it from that has a file, as rustix's
    /// `backend` and the modules serde's `crate_root!` declares are.
    #[test]
    fn modules_are_read_from_each_place_a_cfg_dependent_path_may_name() {
        let scan = scan_files(&[
            (
                "lib.rs",
                r#"#[cfg_attr(demo_a, path = "backend/a.rs")]
#[cfg_attr(demo_b, path = "backend/b.rs")]
mod backend;
#[cfg_attr(demo_a, path = "elsewhere.rs")]
mod both;
#[cfg_attr(demo_a, path = "nowhere.rs")]
mod neither;
#[cfg_attr(demo_a, path = "inline")]
mod inline {}
"#,
            ),
            ("backend/a.rs", "pub unsafe fn a() {}\n"),
            ("backend/b.rs", "pub unsafe fn b() {}\n"),
            ("elsewhere.rs", "pub unsafe fn c() {}\n"),
            ("both.rs", "pub unsafe fn d() {}\n"),
        ]);
        let neither = "module `neither` has no file at any place its #[path], which depends on an unknown cfg, may put it";
        let inline = "module `inline` has a #[path] that depends on an unknown cfg";
        assert_eq!(
            scan.unscanned,
            [
                unscanned("src/lib.rs", 7, neither),
                unscanned("src/lib.rs", 9, inline)
            ]
        );
        assert_eq!(scan.unsafe_code.functions, 4);
    }

    /// The `macro_rules!` of each file a `#[macro_use]` module may be in
    /// may be the ones a later call names.
    #[test]
    fn macros_of_alternative_module_files_do_not_hide_each_other() {
        let scan = scan_files(&[
            (
                "lib.rs",
                "#[macro_use]\n#[cfg_attr(demo_a, path = \"other.rs\")]\nmod macros;\nroot!();\n",
            ),
            ("macros.rs", "macro_rules! root { () => { mod a; }; }\n"),
            ("other.rs", "macro_rules! root { () => { mod b; }; }\n"),
            ("a.rs", "pub unsafe fn a() {}\n"),
            ("b.rs", "pub unsafe fn b() {}\n"),
        ]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }
}
