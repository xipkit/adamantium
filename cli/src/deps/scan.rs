use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map::Entry},
    fs, mem,
    path::{Component, Path, PathBuf},
    slice,
};

use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use serde::{Deserialize, Serialize};
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
    pub name: &'a str,
    pub root: &'a Path,
    pub src: &'a Path,
    pub edition_2015: bool,
    pub externs: &'a BTreeMap<String, String>,
    pub dependencies: BTreeMap<String, usize>,
    pub cfg: Cfg<'a>,
}

/// The kinds of `unsafe` code, counted where they are written.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct UnsafeCounts {
    pub functions: usize,
    pub blocks: usize,
    pub impls: usize,
    pub traits: usize,
    pub extern_blocks: usize,
    pub attributes: usize,
    pub global_asm: usize,
    pub in_macros: usize,
}

impl UnsafeCounts {
    /// Counts all unsafe sites in the scan.
    pub fn total(&self) -> usize {
        self.by_category().into_iter().map(|(_, count)| count).sum()
    }

    /// The names used in the dependency report and each category's count.
    pub fn by_category(&self) -> [(&'static str, usize); 8] {
        [
            ("functions", self.functions),
            ("blocks", self.blocks),
            ("impls", self.impls),
            ("traits", self.traits),
            ("extern_blocks", self.extern_blocks),
            ("attributes", self.attributes),
            ("global_asm", self.global_asm),
            ("in_macros", self.in_macros),
        ]
    }
}

/// A place in a dependency's source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

/// Source the scan couldn't read, so its counts leave it out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Unscanned {
    pub file: String,
    pub line: Option<usize>,
    pub reason: String,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct Scan {
    pub forbids_unsafe: bool,
    pub unsafe_code: UnsafeCounts,
    pub calls: BTreeMap<usize, Vec<Site>>,
    pub unresolved: BTreeMap<String, usize>,
    pub unscanned: Vec<Unscanned>,
}

/// Scans a library's module tree. `entry` is its index in `crates`.
pub fn scan(crates: &[Crate], entry: usize, disallowed: &Disallowed) -> Scan {
    let Some(krate) = crates.get(entry) else {
        return Scan::default();
    };
    let mut scanner = Scanner::read(crates, entry, krate, disallowed, false);
    scanner.scan.forbids_unsafe |= manifest_forbids_unsafe(krate.root);
    scanner.definitions = scanner.definitions();
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
                hidden: HashSet::new(),
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
                hidden: deferred.hidden,
            };
            visitor.tokens(deferred.tokens);
        }
    }
    drop(visited);
    proc_macro2::extra::invalidate_current_thread_spans();
    scanner.scan
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
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
    fn alternatives(&self) -> &[Self] {
        match self {
            Self::Either(alternatives) => alternatives,
            other => slice::from_ref(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Namespace {
    Types,
    Values,
    Macros,
}

#[derive(Debug, PartialEq, Eq)]
struct Bound {
    binding: Binding,
    certain: bool,
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
        self.binding = match <[Binding; 1]>::try_from(alternatives) {
            Ok([only]) => only,
            Err(alternatives) => Binding::Either(alternatives),
        };
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Scope {
    types: HashMap<String, Bound>,
    values: HashMap<String, Bound>,
    macros: HashMap<String, Bound>,
    globs: bool,
}

impl Scope {
    fn map(&mut self, namespace: Namespace) -> &mut HashMap<String, Bound> {
        match namespace {
            Namespace::Types => &mut self.types,
            Namespace::Values => &mut self.values,
            Namespace::Macros => &mut self.macros,
        }
    }

    fn bound(&self, namespace: Namespace, name: &str) -> Option<&Bound> {
        let map = match namespace {
            Namespace::Types => &self.types,
            Namespace::Values => &self.values,
            Namespace::Macros => &self.macros,
        };
        map.get(name)
    }

    fn get(&self, namespace: Namespace, name: &str) -> Option<&Binding> {
        self.bound(namespace, name).map(|bound| &bound.binding)
    }

    fn shadow(&mut self, namespace: Namespace, name: String) {
        self.map(namespace).insert(
            name,
            Bound {
                binding: Binding::Opaque,
                certain: true,
            },
        );
    }

    fn define(&mut self, namespace: Namespace, name: String, binding: Binding, certain: bool) {
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

    fn bring_in(&mut self, name: &str, used: &Binding, item: &Binding, keep: bool) {
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
        bound.binding = match <[Binding; 1]>::try_from(alternatives) {
            Ok([only]) => only,
            Err(alternatives) => Binding::Either(alternatives),
        };
    }

    fn merge(&mut self, other: Self) {
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

fn namespace(path: &[String], index: usize, last: Namespace) -> Namespace {
    if index + 1 == path.len() {
        last
    } else {
        Namespace::Types
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Path {
        krate: Option<usize>,
        path: Vec<String>,
    },
    Local,
    Unknown,
    Either(Vec<Resolved>),
}

impl Resolved {
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

    fn map_paths(self, mut f: impl FnMut(Option<usize>, Vec<String>) -> Self) -> Self {
        Self::either(
            self.into_alternatives()
                .into_iter()
                .map(|resolved| match resolved {
                    Self::Path { krate, path } => f(krate, path),
                    other => other,
                }),
        )
    }

    fn join(self, rest: &[String]) -> Self {
        self.map_paths(|krate, mut path| {
            path.extend(rest.iter().cloned());
            Self::Path { krate, path }
        })
    }
}

const MAX_DEPTH: usize = 16;

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

struct Loaded {
    attrs: Vec<Attribute>,
    items: Vec<Item>,
    rel: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
}

struct ModuleFile {
    path: PathBuf,
    mod_rs: bool,
    condition: Option<TokenStream>,
}

enum Location {
    Inline { dir: PathBuf },
    Files(Vec<ModuleFile>),
    Unscannable(String),
}

struct Scanner<'a> {
    crates: &'a [Crate<'a>],
    id: usize,
    krate: &'a Crate<'a>,
    disallowed: &'a Disallowed,
    scan: Scan,
    visited: HashSet<PathBuf>,
    files: Vec<Loaded>,
    modules: HashMap<Vec<String>, Scope>,
    definitions: HashMap<Vec<String>, usize>,
    expanded: HashSet<(String, usize, usize)>,
    unexpanded: HashSet<String>,
    deferred: BTreeMap<(String, usize, usize), Deferred>,
    sources: HashMap<String, String>,
    expansions: usize,
    prelude: Vec<Imported>,
    libraries: HashMap<usize, Scanner<'a>>,
    around: Vec<(Vec<String>, Scope)>,
    foreign: HashSet<String>,
    dollar_crates: HashMap<(String, usize, usize), usize>,
    exports: Option<Vec<Export>>,
    crate_spans: Vec<Span>,
    macro_paths: HashMap<Vec<String>, Vec<MacroRules>>,
    last_pass: HashMap<Vec<String>, Scope>,
    path_calls: Vec<PathCall>,
    instances: HashMap<(String, usize, usize, usize), usize>,
    read_at: HashMap<(String, usize, usize), usize>,
    hidden: HashMap<(String, usize, usize), HashSet<String>>,
}

struct PathCall {
    module: Vec<String>,
    path: syn::Path,
    found: Option<Vec<Found>>,
}

struct Export {
    name: String,
    rules: Option<Rules>,
    rel: String,
    span: Span,
    defined: Truth,
}

struct Deferred {
    name: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
    tokens: TokenStream,
    hidden: HashSet<String>,
}

#[derive(Clone)]
struct MacroRules {
    name: String,
    rules: Option<Rules>,
    rel: String,
    span: Span,
    condition: Vec<TokenStream>,
    certain: bool,
    defined: Truth,
    instance: usize,
}

struct Imported {
    name: String,
    library: usize,
    export: usize,
    condition: Vec<TokenStream>,
    certain: bool,
    defined: Truth,
    import: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    MacroUse(usize),
    Path,
}

#[derive(Clone, PartialEq, Eq)]
enum Found {
    Export {
        library: usize,
        export: usize,
    },
    Own {
        path: Vec<String>,
        definition: usize,
    },
}

struct Candidate<'t> {
    rules: &'t Rules,
    rel: &'t str,
    span: Span,
    condition: Cow<'t, [TokenStream]>,
    defined: Truth,
    origin: Origin,
    instance: Option<usize>,
}

impl Candidate<'_> {
    fn either(&mut self, other: Self) {
        let text = |condition: &[TokenStream]| {
            condition
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        if text(&self.condition) != text(&other.condition) {
            let span = self.span;
            let either = cfg_call(
                "any",
                [
                    cfg_call("all", self.condition.to_vec(), span),
                    cfg_call("all", other.condition.to_vec(), span),
                ],
                span,
            );
            self.condition = Cow::Owned(vec![either]);
        }
        self.defined = self.defined.or(other.defined);
    }
}

#[derive(Default)]
struct Textual {
    macros: Vec<MacroRules>,
    conditions: Vec<TokenStream>,
    depth: usize,
    uncertain: bool,
}

impl Textual {
    fn hides(&self, name: &str) -> bool {
        self.macros
            .iter()
            .any(|definition| definition.name == name && definition.certain)
    }

    fn hidden(&self) -> HashSet<String> {
        self.macros
            .iter()
            .filter(|definition| definition.certain)
            .map(|definition| definition.name.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Place<'p> {
    module: &'p [String],
    mod_dir: &'p Path,
    path_dir: &'p Path,
    rel: &'p str,
}

const MAX_EXPANSIONS: usize = 128;

const MAX_EXPANDED_CALLS: usize = 10_000;

const MAX_PASSES: usize = 8;

impl<'a> Scanner<'a> {
    fn new(
        crates: &'a [Crate<'a>],
        id: usize,
        krate: &'a Crate<'a>,
        disallowed: &'a Disallowed,
    ) -> Self {
        Self {
            crates,
            id,
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
            libraries: HashMap::new(),
            around: Vec::new(),
            foreign: HashSet::new(),
            dollar_crates: HashMap::new(),
            exports: None,
            crate_spans: Vec::new(),
            macro_paths: HashMap::new(),
            last_pass: HashMap::new(),
            path_calls: Vec::new(),
            instances: HashMap::new(),
            read_at: HashMap::new(),
            hidden: HashMap::new(),
        }
    }

    fn read(
        crates: &'a [Crate<'a>],
        id: usize,
        krate: &'a Crate<'a>,
        disallowed: &'a Disallowed,
        exports: bool,
    ) -> Self {
        let root = vec![krate.name.to_owned()];
        let mut scanner = Self::new(crates, id, krate, disallowed);
        let mut passes = 0;
        loop {
            scanner.exports = exports.then(Vec::new);
            scanner.load(krate.src, root.clone(), true, None, &mut Textual::default());
            passes += 1;
            scanner.last_pass.clear();
            if scanner.calls_settled() || passes == MAX_PASSES {
                break;
            }
            let mut next = Self::new(crates, id, krate, disallowed);
            next.libraries = mem::take(&mut scanner.libraries);
            next.sources = mem::take(&mut scanner.sources);
            next.foreign = mem::take(&mut scanner.foreign);
            next.dollar_crates = mem::take(&mut scanner.dollar_crates);
            next.instances = mem::take(&mut scanner.instances);
            next.macro_paths = mem::take(&mut scanner.macro_paths);
            next.last_pass = mem::take(&mut scanner.modules);
            scanner = next;
        }
        scanner
    }

    fn calls_settled(&mut self) -> bool {
        let calls = mem::take(&mut self.path_calls);
        calls
            .iter()
            .all(|call| self.call_macros(&call.module, &call.path, None).0 == call.found)
    }

    fn call_macros(
        &mut self,
        module: &[String],
        path: &syn::Path,
        indexing: Option<&Scope>,
    ) -> (Option<Vec<Found>>, bool) {
        let mut resolver = Resolver::new(self, &[]);
        if let Some(scope) = indexing {
            resolver = resolver.indexing(module, scope);
        }
        let resolved = if is_bare(path) {
            let bound = path.segments.first().and_then(|segment| {
                let name = segment.ident.unraw().to_string();
                resolver.module(module)?.bound(Namespace::Macros, &name)
            });
            let Some(bound) = bound else {
                return (Some(Vec::new()), true);
            };
            let certain = bound.certain;
            let resolved = resolver.binding(module, 0, Namespace::Macros, &bound.binding, 0);
            (resolved, certain)
        } else {
            (resolver.syn_path(module, Namespace::Macros, path), true)
        };
        let (resolved, certain) = resolved;
        (self.path_macros(&resolved), certain)
    }

    fn name_macro(&mut self, path: Vec<String>, definition: MacroRules) {
        let definitions = self.macro_paths.entry(path).or_default();
        let same = definitions
            .iter_mut()
            .find(|known| known.instance == definition.instance);
        match same {
            Some(known) => *known = definition,
            None => definitions.push(definition),
        }
    }

    fn instance(&mut self, rel: &str, span: Span) -> usize {
        let start = span.start();
        let place = (rel.to_owned(), start.line, start.column);
        let before = self.read_at.entry(place.clone()).or_default();
        let key = (place.0, place.1, place.2, *before);
        *before += 1;
        let next = self.instances.len();
        *self.instances.entry(key).or_insert(next)
    }

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
                Item::Mod(item) => {
                    self.around.push((place.module.to_vec(), mem::take(scope)));
                    self.child(item, place, textual);
                    if let Some((_, around)) = self.around.pop() {
                        *scope = around;
                    }
                }
                Item::Use(item) => self.use_macros(item, place, scope, textual),
                Item::Macro(call) => {
                    match &call.ident {
                        Some(name) => self.define(call, name, place, scope, textual),
                        None => {
                            if !reads_input(&call.mac) && self.expand(call, place, scope, textual) {
                                continue;
                            }
                        }
                    }
                    let hidden = textual.hidden();
                    match self.hidden.entry(call_site(&call.mac)) {
                        Entry::Occupied(mut known) => {
                            known.get_mut().retain(|name| hidden.contains(name));
                        }
                        Entry::Vacant(place) => {
                            place.insert(hidden);
                        }
                    }
                }
                _ => {}
            }
            indexed.push(item);
        }
        *items = indexed;
    }

    fn add_scope(&mut self, module: &[String], scope: Scope) {
        match self.modules.get_mut(module) {
            Some(existing) => existing.merge(scope),
            None => {
                self.modules.insert(module.to_vec(), scope);
            }
        }
    }

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

    fn keep_macros(&self, attrs: &[Attribute], start: usize, textual: &mut Textual) {
        match self.macro_use(attrs) {
            Truth::True => {}
            Truth::Unknown => {
                for definition in textual.macros.iter_mut().skip(start) {
                    definition.rules = None;
                }
            }
            Truth::False => textual.macros.truncate(start),
        }
    }

    fn macro_use(&self, attrs: &[Attribute]) -> Truth {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .filter(|(_, meta)| matches!(meta, Meta::Path(path) if path.is_ident("macro_use")))
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies))
    }

    fn unscanned(&mut self, unscanned: Unscanned) {
        if !self.scan.unscanned.contains(&unscanned) {
            self.scan.unscanned.push(unscanned);
        }
    }

    fn file_of(&self, span: Span, fallback: &str) -> String {
        self.sources
            .get(&span.file())
            .map_or_else(|| fallback.to_owned(), Clone::clone)
    }

    fn define(
        &mut self,
        item: &ItemMacro,
        name: &Ident,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) {
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
        let rel = self.file_of(name.span(), place.rel);
        let unraw = name.unraw().to_string();
        let rules = Rules::parse(item.mac.tokens.clone());
        let instance = self.instance(&rel, name.span());
        let definition = MacroRules {
            name: unraw.clone(),
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
            instance,
        };
        if self.exports.is_some() {
            self.crate_spans
                .extend(definition.rules.iter().flat_map(Rules::crate_spans));
        }
        let effective = self.krate.cfg.effective(&item.attrs);
        let export = effective
            .iter()
            .filter(|(_, meta)| meta.path().is_ident("macro_export"));
        let exported = export
            .clone()
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies));
        if exported != Truth::False {
            let local_inner_macros = export.clone().any(|(_, meta)| {
                matches!(meta, Meta::List(list) if list
                    .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
                    .is_ok_and(|args| args.iter().any(|arg| arg == "local_inner_macros")))
            });
            let mut rules = definition.rules.clone();
            if local_inner_macros && let Some(rules) = &mut rules {
                rules.local_inner_macros();
                if self.exports.is_some() {
                    self.crate_spans.extend(rules.crate_spans());
                }
            }
            let defined = definition.defined.and(exported);
            if let Some(exports) = &mut self.exports {
                exports.push(Export {
                    name: unraw.clone(),
                    rules: rules.clone(),
                    rel: definition.rel.clone(),
                    span: name.span(),
                    defined: holds.and(exported),
                });
            }
            let crate_root = [self.krate.name.to_owned()];
            let path = vec![self.krate.name.to_owned(), unraw.clone()];
            let certain = definition.certain && exported == Truth::True;
            let root = if place.module.len() == 1 {
                Some(scope)
            } else {
                match self.around.iter_mut().find(|(module, _)| module.len() == 1) {
                    Some((_, root)) => Some(root),
                    None => self.modules.get_mut(crate_root.as_slice()),
                }
            };
            if let Some(root) = root {
                root.define(
                    Namespace::Macros,
                    unraw,
                    Binding::Item(None, path.clone()),
                    certain,
                );
            }
            self.name_macro(
                path,
                MacroRules {
                    rules,
                    certain,
                    defined,
                    ..definition.clone()
                },
            );
        }
        textual.macros.push(definition);
    }

    fn use_macros(
        &mut self,
        item: &syn::ItemUse,
        place: Place,
        scope: &mut Scope,
        textual: &Textual,
    ) {
        if self.krate.edition_2015 || item.leading_colon.is_some() {
            return;
        }
        let certain = match self.krate.cfg.enabled(&item.attrs) {
            Truth::True => !textual.uncertain,
            Truth::Unknown => false,
            Truth::False => return,
        };
        let mut condition = textual.conditions.clone();
        condition.extend(cfg_predicates(&item.attrs));
        let mut names = Vec::new();
        bare_names(&item.tree, &mut names);
        for (name, binds) in names {
            let mut definitions = Vec::new();
            let mut hidden = false;
            for definition in textual.macros.iter().rev() {
                if definition.name != name {
                    continue;
                }
                definitions.push(definition);
                if definition.certain {
                    hidden = true;
                    break;
                }
            }
            if definitions.is_empty() {
                continue;
            }
            let mut path = place.module.to_vec();
            path.push(binds.clone());
            for definition in definitions {
                let mut both = definition.condition.clone();
                both.extend(condition.iter().cloned());
                self.name_macro(
                    path.clone(),
                    MacroRules {
                        condition: both,
                        certain: definition.certain && certain,
                        ..definition.clone()
                    },
                );
            }
            let used = Binding::Use {
                krate: None,
                path: vec![name],
                leading: false,
            };
            scope.bring_in(&binds, &used, &Binding::Item(None, path), !hidden);
        }
    }

    fn import_macros(&mut self, items: &[Item], textual: &Textual) {
        for (import, item) in items.iter().enumerate() {
            let Item::ExternCrate(item) = item else {
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
            let extern_name = item.ident.unraw().to_string();
            let Some(&library) = self.krate.dependencies.get(&extern_name) else {
                continue;
            };
            let Some(exports) = self
                .library(library)
                .and_then(|library| library.exports.as_ref())
            else {
                continue;
            };
            let exports: Vec<_> = exports
                .iter()
                .map(|export| (export.name.clone(), export.defined))
                .collect();
            for (index, (name, exported)) in exports.into_iter().enumerate() {
                let imported = self.imported(&item.attrs, &name);
                if imported == Truth::False || exported == Truth::False {
                    continue;
                }
                let defined = exported.and(imported);
                self.prelude.push(Imported {
                    name,
                    library,
                    export: index,
                    condition: condition.clone(),
                    certain: holds == Truth::True && defined == Truth::True,
                    defined,
                    import,
                });
            }
        }
    }

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

    fn library(&mut self, id: usize) -> Option<&Scanner<'a>> {
        if !self.libraries.contains_key(&id) {
            let krate = self.crates.get(id)?;
            let mut reader = if krate.cfg.proc_macro {
                let mut reader = Scanner::new(self.crates, id, krate, self.disallowed);
                reader.exports = Some(Vec::new());
                reader
            } else {
                Scanner::read(self.crates, id, krate, self.disallowed, true)
            };
            reader.files.clear();
            for (source, rel) in mem::take(&mut reader.sources) {
                self.foreign.insert(source.clone());
                self.sources
                    .insert(source, self.relative(&krate.root.join(rel)));
            }
            for export in reader.exports.iter_mut().flatten() {
                export.rel = self.relative(&krate.root.join(&export.rel));
            }
            for (key, dollar_id) in mem::take(&mut reader.dollar_crates) {
                self.dollar_crates.entry(key).or_insert(dollar_id);
            }
            for span in mem::take(&mut reader.crate_spans) {
                let start = span.start();
                self.dollar_crates
                    .entry((span.file(), start.line, start.column))
                    .or_insert(id);
            }
            self.libraries.insert(id, reader);
        }
        self.libraries.get(&id)
    }

    fn export(&self, library: usize, export: usize) -> Option<&Export> {
        self.libraries.get(&library)?.exports.as_ref()?.get(export)
    }

    fn path_macros(&mut self, resolved: &Resolved) -> Option<Vec<Found>> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for alternative in resolved.alternatives() {
            let Resolved::Path { krate, path } = alternative else {
                return None;
            };
            match krate {
                Some(id) if *id != self.id => {
                    self.collect_foreign_macros(*id, path.clone(), 0, &mut seen, &mut found)?;
                }
                _ if path.first().map(String::as_str) == Some(self.krate.name) => {
                    let count = self.macro_paths.get(path)?.len();
                    for definition in 0..count {
                        let own = Found::Own {
                            path: path.clone(),
                            definition,
                        };
                        if !found.contains(&own) {
                            found.push(own);
                        }
                    }
                }
                _ => return None,
            }
        }
        Some(found)
    }

    fn collect_foreign_macros(
        &mut self,
        id: usize,
        path: Vec<String>,
        depth: usize,
        seen: &mut HashSet<(usize, Vec<String>)>,
        found: &mut Vec<Found>,
    ) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        if !seen.insert((id, path.clone())) {
            return Some(());
        }
        let reader = self.library(id)?;
        let targets = Resolver::new(reader, &[]).step(path, Namespace::Macros, 0);
        for target in targets.into_alternatives() {
            let Resolved::Path {
                krate: target_krate,
                path: target_path,
            } = target
            else {
                return None;
            };
            if let Some(next_id) = target_krate
                && next_id != id
            {
                self.collect_foreign_macros(next_id, target_path, depth + 1, seen, found)?;
                continue;
            }
            let (name, module) = target_path.split_last()?;
            let reader = self.libraries.get(&id)?;
            if module.first().map(String::as_str) != Some(reader.krate.name) {
                return None;
            }
            let scope = reader.modules.get(module)?;
            match scope.get(Namespace::Macros, name) {
                None if !scope.globs => continue,
                Some(_) if module.len() == 1 => {}
                _ => return None,
            }
            let exports = reader.exports.as_ref()?;
            let mut any = false;
            for (export, _) in exports
                .iter()
                .enumerate()
                .filter(|(_, export)| export.name == *name)
            {
                any = true;
                let export = Found::Export {
                    library: id,
                    export,
                };
                if !found.contains(&export) {
                    found.push(export);
                }
            }
            if !any {
                return None;
            }
        }
        Some(())
    }

    fn candidates<'t>(
        &'t self,
        textual: &'t Textual,
        scope: &Scope,
        name: &str,
        imported: Option<(&[Found], bool)>,
    ) -> Option<Vec<Candidate<'t>>> {
        let mut candidates = Vec::new();
        for definition in textual.macros.iter().rev() {
            if definition.name != name {
                continue;
            }
            candidates.push(Candidate {
                rules: definition.rules.as_ref()?,
                rel: &definition.rel,
                span: definition.span,
                condition: Cow::Borrowed(&definition.condition),
                defined: definition.defined,
                origin: Origin::Local,
                instance: Some(definition.instance),
            });
            if definition.certain {
                return Some(candidates);
            }
        }
        let used = imported.map_or(&[][..], |(found, _)| found);
        if let Some((found, certain)) = imported {
            let found: Vec<_> = self
                .found_candidates(found, certain)?
                .into_iter()
                .filter(|candidate| {
                    candidate.instance.is_none()
                        || !candidates
                            .iter()
                            .any(|local| local.instance == candidate.instance)
                })
                .collect();
            candidates.extend(found);
            if certain {
                return Some(candidates);
            }
        }
        let mut prelude = self
            .prelude
            .iter()
            .filter(|imported| imported.name == name)
            .peekable();
        if prelude.peek().is_some() {
            if scope.globs {
                return None;
            }
            let mut hidden = None;
            for imported in prelude.rev() {
                if hidden.is_some_and(|import| imported.import != import) {
                    break;
                }
                let read = used.iter().any(|found| {
                    matches!(found, Found::Export { library, export }
                        if *library == imported.library && *export == imported.export)
                });
                if !read {
                    let export = self.export(imported.library, imported.export)?;
                    candidates.push(Candidate {
                        rules: export.rules.as_ref()?,
                        rel: &export.rel,
                        span: export.span,
                        condition: Cow::Borrowed(&imported.condition),
                        defined: imported.defined,
                        origin: Origin::MacroUse(imported.import),
                        instance: None,
                    });
                }
                if imported.certain {
                    hidden = Some(imported.import);
                }
            }
        }
        (!candidates.is_empty()).then_some(candidates)
    }

    fn found_candidates(&self, found: &[Found], certain: bool) -> Option<Vec<Candidate<'_>>> {
        let mut places = HashSet::new();
        let mut definitions = Vec::new();
        for found in found {
            let definition = match found {
                Found::Export { library, export } => {
                    let export = self.export(*library, *export)?;
                    places.insert((Some(*library), vec![export.name.clone()]));
                    Candidate {
                        rules: export.rules.as_ref()?,
                        rel: &export.rel,
                        span: export.span,
                        condition: Cow::Borrowed(&[]),
                        defined: export.defined,
                        origin: Origin::Path,
                        instance: None,
                    }
                }
                Found::Own { path, definition } => {
                    let definition = self.macro_paths.get(path)?.get(*definition)?;
                    places.insert((None, path.clone()));
                    Candidate {
                        rules: definition.rules.as_ref()?,
                        rel: &definition.rel,
                        span: definition.span,
                        condition: Cow::Borrowed(&definition.condition),
                        defined: definition.defined,
                        origin: Origin::Path,
                        instance: Some(definition.instance),
                    }
                }
            };
            let same = definitions.iter_mut().find(|known: &&mut Candidate| {
                definition.instance.is_some() && known.instance == definition.instance
            });
            match same {
                Some(known) => known.either(definition),
                None => definitions.push(definition),
            }
        }
        let one = certain && places.len() == 1;
        let candidates = definitions
            .into_iter()
            .rev()
            .map(|candidate| Candidate {
                defined: if one {
                    candidate.defined
                } else {
                    Truth::Unknown
                },
                ..candidate
            })
            .collect();
        Some(candidates)
    }

    fn expand(
        &mut self,
        call: &ItemMacro,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) -> bool {
        let path = &call.mac.path;
        let Some(name) = path.segments.last().map(|segment| &segment.ident) else {
            return false;
        };
        if self.krate.cfg.enabled(&call.attrs) == Truth::False {
            return false;
        }
        let bare = is_bare(path);
        let unraw = name.unraw().to_string();
        let found = if bare && textual.hides(&unraw) {
            None
        } else {
            let (found, certain) = self.call_macros(place.module, path, Some(scope));
            self.path_calls.push(PathCall {
                module: place.module.to_vec(),
                path: path.clone(),
                found: found.clone(),
            });
            let Some(found) = found else {
                return false;
            };
            if found.is_empty() {
                if !bare {
                    return false;
                }
                None
            } else {
                Some((found, certain))
            }
        };
        let found = found
            .as_ref()
            .map(|(found, certain)| (&found[..], *certain));
        let candidates = if bare {
            self.candidates(textual, scope, &unraw, found)
        } else {
            found.and_then(|(found, certain)| self.found_candidates(found, certain))
        };
        let Some(candidates) = candidates else {
            return false;
        };
        let file = self.file_of(name.span(), place.rel);
        let start = name.span().start();
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
        let mut later = Vec::new();
        let mut group: Option<(Origin, Option<TokenStream>)> = None;
        let mut expansions = Vec::new();
        for candidate in candidates {
            let span = candidate.span;
            if candidate.origin == Origin::Local
                || group
                    .as_ref()
                    .is_some_and(|(current, _)| *current != candidate.origin)
            {
                later.extend(group.take().and_then(|(_, hides)| hides));
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
            let hides = (candidate.defined == Truth::True)
                .then(|| cfg_call("all", candidate.condition.to_vec(), span));
            match candidate.origin {
                Origin::Local => later.extend(hides),
                origin => {
                    let (_, item) = group.get_or_insert((origin, None));
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
            let Some(mut items) = candidate
                .rules
                .expand(call.mac.tokens.clone())
                .and_then(|tokens| parse_items.parse2(tokens).ok())
            else {
                return false;
            };
            Expand.fragment(&mut items);
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
            let at = (candidate.rel.to_owned(), start.line, start.column);
            expansions.push((items, at, candidate.instance.is_none(), defined));
        }
        self.expansions += 1;
        for (mut items, at, foreign, defined) in expansions {
            let rel = at.0.clone();
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

    fn locate(&self, item: &ItemMod, mod_dir: &Path, path_dir: &Path) -> Location {
        let name = item.ident.unraw().to_string();
        let span = item.ident.span();
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

    fn extern_crate(&self, name: &str) -> Option<(Option<usize>, String)> {
        if let Some(lib) = self.krate.externs.get(name) {
            let id = self.krate.dependencies.get(name).copied();
            return Some((id, lib.clone()));
        }
        SYSROOT.contains(&name).then(|| (None, name.to_owned()))
    }

    fn dollar_crate(&self, ident: &Ident) -> Option<usize> {
        if ident != "crate" {
            return None;
        }
        let span = ident.span();
        let start = span.start();
        self.dollar_crates
            .get(&(span.file(), start.line, start.column))
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
                if let Resolved::Path {
                    path: definition, ..
                } = definition
                    && definition != path
                {
                    definitions.insert(definition, entry);
                }
            }
        }
        definitions
    }

    fn entry(&self, path: &[String]) -> Option<usize> {
        self.disallowed
            .entry(path)
            .or_else(|| self.definitions.get(path).copied())
    }
}

struct Resolver<'s, 'a> {
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
    fn new(scanner: &'s Scanner<'a>, blocks: &'s [Scope]) -> Self {
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

    fn indexing(mut self, module: &'s [String], scope: &'s Scope) -> Self {
        self.indexing = Some((module, scope));
        self
    }

    fn module(&self, module: &[String]) -> Option<&'s Scope> {
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

    fn syn_path(&mut self, module: &[String], namespace: Namespace, path: &syn::Path) -> Resolved {
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
            return match scanner.extern_crate(first) {
                Some((krate, lib)) => Resolved::Path {
                    krate,
                    path: vec![lib],
                }
                .join(rest),
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
        if let Some((krate, lib)) = scanner.extern_crate(first) {
            return Resolved::Path {
                krate,
                path: vec![lib],
            }
            .join(rest);
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

    fn binding(
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
        let unbound = |_: &mut Self| match scanner.extern_crate(first) {
            Some((krate, lib)) => Resolved::Path {
                krate,
                path: vec![lib],
            }
            .join(rest),
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
            if let Some((krate, lib)) = scanner.extern_crate(first) {
                return Resolved::Path {
                    krate,
                    path: vec![lib],
                }
                .join(rest);
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

    fn step(&mut self, path: Vec<String>, namespace: Namespace, depth: usize) -> Resolved {
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

fn call_site(mac: &Macro) -> (String, usize, usize) {
    let span = mac.bang_token.span;
    let start = span.start();
    (span.file(), start.line, start.column)
}

fn is_bare(path: &syn::Path) -> bool {
    path.leading_colon.is_none() && path.segments.len() == 1
}

fn bare_names(tree: &UseTree, names: &mut Vec<(String, String)>) {
    match tree {
        UseTree::Name(name) if name.ident != "self" => {
            let ident = name.ident.unraw().to_string();
            names.push((ident.clone(), ident));
        }
        UseTree::Rename(rename) if rename.ident != "self" && rename.rename != "_" => {
            names.push((
                rename.ident.unraw().to_string(),
                rename.rename.unraw().to_string(),
            ));
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                bare_names(tree, names);
            }
        }
        _ => {}
    }
}

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

struct Expand;

impl Expand {
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
            let input = match item.ident {
                Some(_) => None,
                None => input_items(&item.mac),
            };
            match input {
                Some(mut inner) => {
                    self.items(&mut inner);
                    let outer = item.attrs.clone();
                    expanded.push(Item::Macro(item));
                    for mut inner in inner {
                        if let Some(attrs) = item_attrs_mut(&mut inner) {
                            let own = mem::take(attrs);
                            attrs.extend(outer.iter().cloned());
                            attrs.extend(own);
                        }
                        expanded.push(inner);
                    }
                }
                None => expanded.push(Item::Macro(item)),
            }
        }
        *items = expanded;
    }
}

fn input_items(mac: &Macro) -> Option<Vec<Item>> {
    if is_include(mac) || mac.tokens.is_empty() {
        return None;
    }
    let parsed = if is_cfg_if(mac) {
        mac.parse_body_with(parse_cfg_if)
    } else if is_cfg_select(mac) {
        mac.parse_body_with(parse_cfg_select)
    } else {
        mac.parse_body_with(parse_items)
    };
    parsed.ok()
}

fn reads_input(mac: &Macro) -> bool {
    input_items(mac).is_some()
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

const GLOBAL_ASM: [&str; 3] = ["std", "arch", "global_asm"];

fn is_unsafe_attribute(meta: &Meta) -> bool {
    let path = meta.path();
    path.is_ident("unsafe") || UNSAFE_ATTRIBUTES.iter().any(|name| path.is_ident(name))
}

struct FileVisitor<'s, 'a> {
    scanner: &'s mut Scanner<'a>,
    file: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
    blocks: Vec<Scope>,
    attributes: HashSet<Site>,
    hidden: HashSet<String>,
}

impl FileVisitor<'_, '_> {
    fn enabled(&self, attrs: &[Attribute]) -> bool {
        self.scanner.krate.cfg.enabled(attrs) != Truth::False
    }

    fn site(&self, span: Span) -> Site {
        let start = span.start();
        Site {
            file: self.scanner.file_of(span, &self.file),
            line: start.line,
            column: start.column + 1,
        }
    }

    fn here(&self, span: Span) -> bool {
        !self.scanner.foreign.contains(&span.file())
    }

    fn writes(&self, unsafety: Option<&Token![unsafe]>) -> bool {
        unsafety.is_some_and(|token| self.here(token.span))
    }

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

    fn unexpanded(&mut self, mac: &Macro) {
        if let Some(segment) = mac.path.segments.last() {
            self.scanner
                .unexpanded
                .insert(segment.ident.unraw().to_string());
        }
    }

    fn calls_global_asm(&self, path: &[String], leading: bool, hidden: &HashSet<String>) -> bool {
        let bare = !leading && path.len() == 1;
        if bare && path.first().is_some_and(|name| hidden.contains(name)) {
            return false;
        }
        let named = path.last().is_some_and(|name| name == "global_asm");
        let mut resolver = Resolver::new(self.scanner, &self.blocks);
        let resolved = resolver.path(&self.module, Namespace::Macros, path, leading);
        let globs = || {
            self.blocks
                .iter()
                .chain(resolver.module(&self.module))
                .any(|scope| scope.globs)
        };
        resolved
            .alternatives()
            .iter()
            .any(|resolved| match resolved {
                Resolved::Path { krate: None, path } => canonical(path.clone()) == GLOBAL_ASM,
                Resolved::Unknown => named && (!bare || globs()),
                Resolved::Path { .. } | Resolved::Local | Resolved::Either(_) => false,
            })
    }

    fn item_call(&mut self, mac: &Macro, hidden: &HashSet<String>) {
        if self.here(mac.bang_token.span)
            && self.calls_global_asm(
                &segments(&mac.path),
                mac.path.leading_colon.is_some(),
                hidden,
            )
        {
            self.scanner.scan.unsafe_code.global_asm += 1;
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
        let resolved = match &expr.qself {
            Some(qself) if qself.position == 0 => {
                let base = match &*qself.ty {
                    Type::Slice(_) => Resolved::Path {
                        krate: None,
                        path: vec!["slice".to_owned()],
                    },
                    Type::Path(ty) if ty.qself.is_none() => {
                        resolver.syn_path(&self.module, Namespace::Types, &ty.path)
                    }
                    _ => Resolved::Unknown,
                };
                base.join(&names)
            }
            _ => resolver.syn_path(&self.module, Namespace::Values, &expr.path),
        };
        resolved.map_paths(|krate, path| Resolved::Path {
            krate,
            path: canonical(path),
        })
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

    fn tokens(&mut self, stream: TokenStream) {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        for (index, tree) in trees.iter().enumerate() {
            let previous = index.checked_sub(1).and_then(|index| trees.get(index));
            let before = index.checked_sub(2).and_then(|index| trees.get(index));
            match tree {
                TokenTree::Ident(ident) if ident == "unsafe" && self.here(ident.span()) => {
                    self.scanner.scan.unsafe_code.in_macros += 1;
                }
                TokenTree::Punct(punct) if punct.as_char() == ';' => {
                    if let (Some(TokenTree::Ident(name)), Some(TokenTree::Ident(keyword))) =
                        (previous, before)
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
                        (previous, before)
                        && bang.as_char() == '!'
                        && name != "macro_rules"
                    {
                        self.scanner.unexpanded.insert(name.unraw().to_string());
                        if self.here(name.span())
                            && let Some((path, leading)) =
                                trees.get(..index.saturating_sub(1)).and_then(trailing_path)
                            && self.calls_global_asm(&path, leading, &self.hidden)
                        {
                            self.scanner.scan.unsafe_code.in_macros += 1;
                        }
                    }
                    if group.delimiter() == Delimiter::Bracket
                        && is_attribute(previous, before)
                        && self.here(group.span())
                    {
                        self.scanner.scan.unsafe_code.in_macros +=
                            self.bare_unsafe_attributes(group.stream());
                    }
                    if group.delimiter() == Delimiter::Parenthesis
                        && let Some(TokenTree::Ident(name)) = previous
                        && self.here(name.span())
                    {
                        let name = name.unraw().to_string();
                        let disallowed = self.scanner.disallowed;
                        let counts = match before {
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
        }
    }

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
            (None, _) if self.blocks.is_empty() => {}
            (None, Location::Files(files)) => {
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

fn trailing_path(mut trees: &[TokenTree]) -> Option<(Vec<String>, bool)> {
    let mut path = Vec::new();
    let leading = loop {
        let [rest @ .., TokenTree::Ident(ident)] = trees else {
            if path.is_empty() {
                return None;
            }
            break true;
        };
        path.push(ident.unraw().to_string());
        match rest {
            [rest @ .., TokenTree::Punct(first), TokenTree::Punct(second)]
                if first.as_char() == ':'
                    && first.spacing() == Spacing::Joint
                    && second.as_char() == ':' =>
            {
                trees = rest;
            }
            _ => break false,
        }
    };
    path.reverse();
    Some((path, leading))
}

fn is_attribute(previous: Option<&TokenTree>, before: Option<&TokenTree>) -> bool {
    is_punct(previous, '#') || (is_punct(previous, '!') && is_punct(before, '#'))
}

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
        let hidden = self
            .scanner
            .hidden
            .get(&call_site(&item.mac))
            .cloned()
            .unwrap_or_default();
        if item.ident.is_none() {
            self.item_call(&item.mac, &hidden);
            if reads_input(&item.mac) {
                return;
            }
        }
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        match &item.ident {
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
                        hidden,
                    };
                    self.scanner.deferred.insert(at, deferred);
                    return;
                }
            }
            None => self.unexpanded(&item.mac),
        }
        let outer = mem::replace(&mut self.hidden, hidden);
        self.tokens(item.mac.tokens.clone());
        self.hidden = outer;
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
        let attrs = match stmt {
            Stmt::Local(local) => &local.attrs[..],
            Stmt::Macro(mac) => &mac.attrs,
            Stmt::Expr(..) | Stmt::Item(_) => &[],
        };
        if self.enabled(attrs) {
            visit::visit_stmt(self, stmt);
        }
    }

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

    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        let entries: BTreeSet<usize> = self
            .resolve(expr)
            .alternatives()
            .iter()
            .filter_map(|resolved| match resolved {
                Resolved::Path { path, .. } => self.scanner.entry(path),
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
                    Resolved::Path { path, .. } => {
                        self.scanner.entry(path).is_none()
                            && path
                                .first()
                                .is_none_or(|krate| !disallowed.names_crate(krate))
                    }
                    Resolved::Local => false,
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
            .filter_map(|item| {
                let name = match item {
                    Item::Fn(item) => item.sig.ident.to_string(),
                    Item::Struct(item) => item.ident.to_string(),
                    Item::Macro(call) if reads_input(&call.mac) => {
                        return None;
                    }
                    _ => panic!("an item that isn't a function, struct or flattened call"),
                };
                Some((name, cfg.enabled(item_attrs(item))))
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
            .filter_map(|item| {
                let name = match item {
                    Item::Fn(item) => item.sig.ident.to_string(),
                    Item::Struct(item) => item.ident.to_string(),
                    Item::Macro(call) if reads_input(&call.mac) => {
                        return None;
                    }
                    _ => panic!("an item that isn't a function, struct or flattened call"),
                };
                Some((name, cfg.enabled(item_attrs(item))))
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

    #[test]
    fn trailing_path_reads_a_path_back_from_its_end() {
        for (source, path) in [
            (
                "core::arch::global_asm",
                Some((&["core", "arch", "global_asm"][..], false)),
            ),
            (
                "::std::arch::global_asm",
                Some((&["std", "arch", "global_asm"], true)),
            ),
            (
                "$crate::reexported",
                Some((&["crate", "reexported"], false)),
            ),
            ("let x: ::core::name", Some((&["core", "name"], true))),
            ("a; b::c", Some((&["b", "c"], false))),
            ("r#global_asm", Some((&["global_asm"], false))),
            ("x.", None),
            ("", None),
        ] {
            let trees: Vec<TokenTree> =
                source.parse::<TokenStream>().unwrap().into_iter().collect();
            let expected = path.map(|(path, leading)| {
                (
                    path.iter().map(|segment| segment.to_string()).collect(),
                    leading,
                )
            });
            assert_eq!(trailing_path(&trees), expected, "{source}");
        }
    }

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

    fn scan_files(files: &[(&str, &str)]) -> Scan {
        scan_with(files, false, &[])
    }

    #[derive(Clone, Copy)]
    struct Library<'t> {
        name: &'t str,
        features: &'t [&'t str],
        files: &'t [(&'t str, &'t str)],
    }

    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join("src").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    fn scan_with(files: &[(&str, &str)], edition_2015: bool, libraries: &[Library]) -> Scan {
        let target = Target::parse("unix\ntarget_os=\"linux\"\n");
        let no_externs = BTreeMap::new();
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
        let mut crates: Vec<_> = libraries
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
                dependencies: BTreeMap::new(),
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
            .enumerate()
            .map(|(index, library)| (library.name.to_owned(), index))
            .collect();
        let dir = package(files);
        let features = Features::default();
        let src = dir.path().join("src/lib.rs");
        let entry = crates.len();
        crates.push(Crate {
            name: "demo",
            root: dir.path(),
            src: &src,
            edition_2015,
            externs: &externs,
            dependencies,
            cfg: Cfg {
                target: &target,
                features: &features,
                proc_macro: false,
            },
        });
        scan(&crates, entry, &Disallowed::bundled().unwrap())
    }

    fn unscanned(file: &str, line: usize, reason: &str) -> Unscanned {
        Unscanned {
            file: file.to_owned(),
            line: Some(line),
            reason: reason.to_owned(),
        }
    }

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

    const PSM: Library = Library {
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

    #[test]
    fn calls_of_macros_another_crate_exports_are_expanded() {
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
            &[PSM],
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

    #[test]
    fn local_inner_macros_call_their_crates_macros() {
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
        let reason = "module `mine` is declared in a macro body the scan can't read";
        assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 4, reason)]);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    #[test]
    fn calls_of_other_crates_macros_by_path_are_expanded() {
        let lib = r"psm::psm_stack_manipulation! {
    yes { mod guard; }
    no { mod fallback; }
}

mod inner {
    ::psm::psm_stack_manipulation! {
        yes { mod guard; }
        no { mod fallback; }
    }
}

extern crate psm as stack;

stack::psm_stack_manipulation! {
    yes { mod renamed; }
    no { mod renamed_fallback; }
}
";
        let files = [
            ("lib.rs", lib),
            ("guard.rs", "pub unsafe fn a() {}\n"),
            ("fallback.rs", "pub unsafe fn b() {}\n"),
            ("inner/guard.rs", "pub unsafe fn c() {}\n"),
            ("inner/fallback.rs", "pub unsafe fn d() {}\n"),
            ("renamed.rs", "pub unsafe fn e() {}\n"),
            ("renamed_fallback.rs", "pub unsafe fn f() {}\n"),
        ];
        for edition_2015 in [false, true] {
            let scan = scan_with(&files, edition_2015, &[PSM]);
            assert_eq!(scan.unscanned, [], "2015: {edition_2015}");
            assert_eq!(scan.unsafe_code.functions, 6, "2015: {edition_2015}");
        }
    }

    #[test]
    fn calls_of_other_crates_macros_a_use_brings_in_are_expanded() {
        let scan = scan_with(
            &[
                ("lib.rs", "mod named;\nmod renamed;\n"),
                (
                    "named.rs",
                    "use psm::psm_stack_manipulation;\n\npsm_stack_manipulation! {\n    yes { mod guard; }\n    no { mod fallback; }\n}\n",
                ),
                (
                    "renamed.rs",
                    "use psm::psm_stack_manipulation as stack;\n\nstack! {\n    yes { mod guard; }\n    no { mod fallback; }\n}\n",
                ),
                ("named/guard.rs", "pub unsafe fn a() {}\n"),
                ("named/fallback.rs", "pub unsafe fn b() {}\n"),
                ("renamed/guard.rs", "pub unsafe fn c() {}\n"),
                ("renamed/fallback.rs", "pub unsafe fn d() {}\n"),
            ],
            false,
            &[PSM],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 4);
    }

    #[test]
    fn macros_a_use_brings_in_come_between_textual_scope_and_the_prelude() {
        let library = |name, files| Library {
            name,
            features: &[],
            files,
        };
        let picks = library(
            "picks",
            &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { mod theirs; }; }\n",
            )],
        );
        let prelude = library(
            "prelude",
            &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { () => { mod prelude; }; }\n",
            )],
        );
        let modules = ["imported", "maybe", "textual", "unknown"];
        let mut files = vec![
            (
                "lib.rs".to_owned(),
                "#[macro_use]\nextern crate prelude;\n\nmod imported;\nmod maybe;\nmod textual;\nmod unknown;\n"
                    .to_owned(),
            ),
            (
                "imported.rs".to_owned(),
                "use picks::pick;\npick!();\n".to_owned(),
            ),
            (
                "maybe.rs".to_owned(),
                "#[cfg(demo_custom)]\nuse picks::pick;\npick!();\n".to_owned(),
            ),
            (
                "textual.rs".to_owned(),
                "use picks::pick;\nmacro_rules! pick { () => { mod own; }; }\npick!();\n".to_owned(),
            ),
            (
                "unknown.rs".to_owned(),
                "use other::pick;\nmacro_rules! pick { () => { mod own; }; }\npick!();\n".to_owned(),
            ),
        ];
        for module in modules {
            for child in ["theirs", "prelude", "own"] {
                files.push((
                    format!("{module}/{child}.rs"),
                    "pub fn run() {\n    std::thread::spawn(|| ());\n}\n".to_owned(),
                ));
            }
        }
        let files: Vec<_> = files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let scan = scan_with(&files, false, &[picks, prelude]);
        assert_eq!(scan.unscanned, []);
        let scanned: BTreeSet<_> = scan
            .calls
            .values()
            .flatten()
            .map(|site| site.file.as_str())
            .collect();
        assert_eq!(
            scanned,
            BTreeSet::from([
                "src/imported/theirs.rs",
                "src/maybe/prelude.rs",
                "src/maybe/theirs.rs",
                "src/textual/own.rs",
                "src/unknown/own.rs",
            ])
        );
    }

    #[test]
    fn paths_to_other_crates_macros_follow_their_reexports() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub mod macros {
    pub use crate::pick as choose;
}

mod functions {
    pub fn pick() {}
}

pub use functions::pick;
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "picks::macros::choose! { yes { mod chosen; } }\npicks::pick! { yes { mod picked; } }\n",
                ),
                ("chosen.rs", "pub unsafe fn a() {}\n"),
                ("picked.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn macro_paths_the_scan_cannot_follow_are_not_expanded() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub fn spawn() {}
",
            )],
        };
        let scan = scan_with(
            &[(
                "lib.rs",
                r"other::pick! { yes { mod other; } }
picks::spawn! { yes { mod spawn; } }
picks::missing! { yes { mod missing; } }
pub fn own() {}
crate::own! { yes { mod own; } }
",
            )],
            false,
            &[picks],
        );
        let reason =
            |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
        let listed = [(1, "other"), (2, "spawn"), (3, "missing"), (5, "own")]
            .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
        assert_eq!(scan.unscanned, listed);
    }

    #[test]
    fn macros_a_use_and_the_prelude_both_bring_in_are_read_once() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
            )],
        };
        let scan = scan_with(
            &[(
                "lib.rs",
                "#[macro_use]\nextern crate picks;\n\n#[cfg(demo_custom)]\nuse picks::pick;\n\npick! { yes { pub unsafe fn f() {} } }\n",
            )],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    #[test]
    fn macros_a_parent_module_brings_in_are_followed() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "use picks::pick;\n\nmod child;\n\nmod inline {\n    use super::pick;\n    pick! { yes { mod body; } }\n}\n",
                ),
                (
                    "child.rs",
                    "use super::pick;\npick! { yes { mod body; } }\n",
                ),
                ("child/body.rs", "pub unsafe fn a() {}\n"),
                ("inline/body.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn uses_of_other_items_do_not_hide_macros() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n\npub fn spawn() {}\n",
            )],
        };
        let prelude = Library {
            name: "prelude",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! spawn { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate prelude;\n\nmod both;\nmod function;\n",
                ),
                (
                    "both.rs",
                    "use picks::pick;\nuse picks::spawn as pick;\npick! { yes { mod body; } }\n",
                ),
                ("both/body.rs", "pub unsafe fn a() {}\n"),
                (
                    "function.rs",
                    "use picks::spawn;\nspawn! { yes { mod body; } }\n",
                ),
                ("function/body.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[picks, prelude],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn macros_reached_two_ways_are_read_once() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub use crate::pick as alias;
",
            )],
        };
        let scan = scan_with(
            &[(
                "lib.rs",
                r"#[cfg(demo_custom)]
use picks::pick;
#[cfg(not(demo_custom))]
use picks::alias as pick;

pick! { yes { pub unsafe fn f() {} } }
",
            )],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    #[test]
    fn macros_of_dependencies_named_like_keywords_are_expanded() {
        let keyword = Library {
            name: "async",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::inner! { yes { mod $name; } } };
}

#[macro_export]
macro_rules! inner {
    (yes { $($body:tt)* }) => { $($body)* };
}
",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    "#[macro_use]\nextern crate r#async;\n\nouter! { first }\nr#async::outer! { second }\n",
                ),
                ("first.rs", "pub unsafe fn a() {}\n"),
                ("second.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[keyword],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn crate_paths_in_other_crates_macros_call_their_macros() {
        let runtime = Library {
            name: "runtime",
            features: &[],
            files: &[(
                "lib.rs",
                r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::inner! { yes { mod $name; } } };
}

#[macro_export]
macro_rules! inner {
    (yes { $($body:tt)* }) => { $($body)* pub unsafe fn theirs() {} };
}
",
            )],
        };
        let scan = scan_with(
            &[
                ("lib.rs", "runtime::outer! { body }\n"),
                ("body.rs", "pub unsafe fn mine() {}\n"),
            ],
            false,
            &[runtime],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

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

    #[test]
    fn calls_of_the_crates_own_macros_by_path_are_expanded() {
        let mut files = vec![
            (
                "lib.rs",
                r"#[macro_use]
mod macros;

mod uses;

crate::declare!(root_path);
self::declare!(self_path);
macros::local!(module_path);
crate::macros::local!(crate_path);
backend!(dollar_crate);
",
            ),
            (
                "macros.rs",
                r"#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        mod $name;
    };
}

macro_rules! local {
    ($name:ident) => {
        mod $name;
    };
}

pub(crate) use local;

macro_rules! backend {
    ($name:ident) => {
        $crate::declare!($name);
    };
}
",
            ),
            (
                "uses.rs",
                "use crate::macros::local;\nuse crate::declare as renamed;\n\nlocal!(used);\nrenamed!(renamed);\n",
            ),
        ];
        let modules = [
            "root_path.rs",
            "self_path.rs",
            "module_path.rs",
            "crate_path.rs",
            "dollar_crate.rs",
            "uses/used.rs",
            "uses/renamed.rs",
        ];
        files.extend(modules.map(|module| (module, "pub unsafe fn f() {}\n")));
        let scan = scan_files(&files);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, modules.len());
    }

    #[test]
    fn macro_paths_through_names_bound_later_are_followed() {
        let picks = Library {
            name: "picks",
            features: &[],
            files: &[(
                "lib.rs",
                "#[macro_export]\nmacro_rules! pick { ($name:ident) => { mod $name; }; }\n",
            )],
        };
        let scan = scan_with(
            &[
                (
                    "lib.rs",
                    r"use later::pick;

pick!(body);
crate::declare!(early);
crate::later::local!(nested);

mod later {
    pub use picks::pick;

    macro_rules! local {
        ($name:ident) => {
            mod $name;
        };
    }

    pub(crate) use local;
}

#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        mod $name;
    };
}
",
                ),
                ("body.rs", "pub unsafe fn a() {}\n"),
                ("early.rs", "pub unsafe fn b() {}\n"),
                ("nested.rs", "pub unsafe fn c() {}\n"),
            ],
            false,
            &[picks],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 3);
    }

    #[test]
    fn the_crates_own_macros_by_path_are_read_as_each_definition() {
        let scan = scan_files(&[(
            "lib.rs",
            r"#[cfg(demo_custom)]
#[macro_export]
macro_rules! pick {
    ($name:ident) => {
        pub unsafe fn $name() {}
    };
}

#[cfg(not(demo_custom))]
#[macro_export]
macro_rules! pick {
    ($name:ident) => {
        pub fn $name() {}
        unsafe impl Send for crate::Handle {}
    };
}

pub struct Handle;

crate::pick!(by_path);
pick!(by_name);
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(
            scan.unsafe_code,
            UnsafeCounts {
                functions: 2,
                impls: 2,
                ..UnsafeCounts::default()
            }
        );
    }

    #[test]
    fn macros_one_macro_writes_by_each_call_are_each_a_definition() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! make {
    (yes $body:item) => {
        #[macro_export]
        macro_rules! pick {
            () => {
                $body
            };
        }
    };
}

#[cfg(demo_custom)]
make!(yes pub unsafe fn f() {});
#[cfg(not(demo_custom))]
make!(yes pub fn f() {});

mod user {
    crate::pick!();
}
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    #[test]
    fn the_crates_own_macros_by_path_include_later_definitions() {
        let scan = scan_files(&[(
            "lib.rs",
            r"#[cfg(demo_custom)]
#[macro_export]
macro_rules! pick {
    () => {
        pub fn f() {}
    };
}

mod first {
    crate::pick!();
}

#[cfg(not(demo_custom))]
#[macro_export]
macro_rules! pick {
    () => {
        pub unsafe fn f() {}
    };
}

mod second {
    crate::pick!();
}
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn calls_through_modules_of_one_path_read_each_reexport() {
        let scan = scan_files(&[(
            "lib.rs",
            r"#[macro_export]
macro_rules! safe {
    () => {
        pub fn f() {}
    };
}

#[macro_export]
macro_rules! danger {
    () => {
        pub unsafe fn f() {}
    };
}

#[cfg(demo_custom)]
mod choices {
    pub use crate::safe as pick;
}

mod first {
    crate::choices::pick!();
}

#[cfg(not(demo_custom))]
mod choices {
    pub use crate::danger as pick;
}

mod second {
    crate::choices::pick!();
}
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn definitions_written_at_one_place_are_told_apart() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! make {
    (yes $body:item) => {
        #[macro_export]
        macro_rules! pick {
            () => {
                $body
            };
        }
    };
}

#[cfg(demo_custom)]
mod hidden {
    make!(yes pub unsafe fn f() {});
}

#[cfg(not(demo_custom))]
make!(yes pub fn f() {});

pick!();

mod user {
    crate::pick!();
}
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn definitions_named_more_than_one_way_are_read_once() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! m {
    () => {
        pub unsafe fn f() {}
    };
}

mod a {
    pub(crate) use m;
}

mod b {
    pub(crate) use m;
}

#[cfg(demo_custom)]
use a::m as chosen;
#[cfg(not(demo_custom))]
use b::m as chosen;

chosen!();
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    #[test]
    fn definitions_keep_their_identity_across_readings() {
        let scan = scan_files(&[(
            "lib.rs",
            r"macro_rules! m {
    () => {
        pub unsafe fn f() {}
    };
}

mod a {
    pub(crate) use m;
}

#[cfg(demo_custom)]
use a::m as chosen;
#[cfg(not(demo_custom))]
use b::m as chosen;

chosen!();

mod b {
    pub(crate) use m;
}
",
        )]);
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
    }

    struct GraphCrate<'a> {
        name: &'a str,
        deps: &'a [(&'a str, usize)],
        files: &'a [(&'a str, &'a str)],
    }

    fn scan_graph(files: &[(&str, &str)], demo_deps: &[usize], others: &[GraphCrate]) -> Scan {
        let target = Target::parse("unix\ntarget_os=\"linux\"\n");
        let dirs: Vec<_> = others.iter().map(|krate| package(krate.files)).collect();
        let srcs: Vec<_> = dirs
            .iter()
            .map(|dir| dir.path().join("src/lib.rs"))
            .collect();
        let features = Features::default();
        let extern_maps: Vec<BTreeMap<String, String>> = others
            .iter()
            .map(|krate| {
                krate
                    .deps
                    .iter()
                    .map(|(alias, target)| ((*alias).to_owned(), others[*target].name.to_owned()))
                    .collect()
            })
            .collect();
        let mut crates: Vec<Crate> = others
            .iter()
            .enumerate()
            .map(|(index, krate)| Crate {
                name: krate.name,
                root: dirs[index].path(),
                src: &srcs[index],
                edition_2015: false,
                externs: &extern_maps[index],
                dependencies: krate
                    .deps
                    .iter()
                    .map(|(alias, target)| ((*alias).to_owned(), *target))
                    .collect(),
                cfg: Cfg {
                    target: &target,
                    features: &features,
                    proc_macro: false,
                },
            })
            .collect();
        let demo_externs: BTreeMap<String, String> = demo_deps
            .iter()
            .map(|&index| (others[index].name.to_owned(), others[index].name.to_owned()))
            .collect();
        let demo_dependencies: BTreeMap<String, usize> = demo_deps
            .iter()
            .map(|&index| (others[index].name.to_owned(), index))
            .collect();
        let dir = package(files);
        let src = dir.path().join("src/lib.rs");
        let entry = crates.len();
        crates.push(Crate {
            name: "demo",
            root: dir.path(),
            src: &src,
            edition_2015: false,
            externs: &demo_externs,
            dependencies: demo_dependencies,
            cfg: Cfg {
                target: &target,
                features: &features,
                proc_macro: false,
            },
        });
        scan(&crates, entry, &Disallowed::bundled().unwrap())
    }

    #[test]
    fn reexported_macros_from_another_crate_are_expanded() {
        let inner_files = [(
            "lib.rs",
            r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::helper! { mod $name; } };
}

#[macro_export]
macro_rules! helper {
    ($($body:tt)*) => { $($body)* pub unsafe fn theirs() {} };
}
",
        )];
        let facade_files = [("lib.rs", "pub use aliased_inner::outer as select;\n")];
        let scan = scan_graph(
            &[
                (
                    "lib.rs",
                    "facade::select! { direct }\nuse facade::select;\nselect! { imported }\n",
                ),
                ("direct.rs", "pub unsafe fn a() {}\n"),
                ("imported.rs", "pub unsafe fn b() {}\n"),
            ],
            &[1],
            &[
                GraphCrate {
                    name: "inner",
                    deps: &[],
                    files: &inner_files,
                },
                GraphCrate {
                    name: "facade",
                    deps: &[("aliased_inner", 0)],
                    files: &facade_files,
                },
            ],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }

    #[test]
    fn use_dollar_crate_in_reexported_macro_resolves_to_defining_crate() {
        let inner_files = [(
            "lib.rs",
            r"#[macro_export]
macro_rules! make {
    ($name:ident, $body:block) => {
        use {$crate::helper, std::mem::forget};
        helper! { mod $name; }
        fn check() $body
    };
}

#[macro_export]
macro_rules! helper {
    ($($body:tt)*) => { $($body)* };
}
",
        )];
        let facade_files = [("lib.rs", "pub use inner::make;\n")];
        let scan = scan_graph(
            &[
                ("lib.rs", "facade::make! { generated, { forget(()); } }\n"),
                ("generated.rs", "pub unsafe fn a() {}\n"),
            ],
            &[1],
            &[
                GraphCrate {
                    name: "inner",
                    deps: &[],
                    files: &inner_files,
                },
                GraphCrate {
                    name: "facade",
                    deps: &[("inner", 0)],
                    files: &facade_files,
                },
            ],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 1);
        assert_eq!(scan.calls.len(), 1);
    }

    #[test]
    fn reexported_module_macros_follow_into_third_crate() {
        let inner_files = [(
            "lib.rs",
            r"#[macro_export]
macro_rules! leaf {
    ($name:ident) => { mod $name; };
}
",
        )];
        let facade_files = [(
            "lib.rs",
            r"pub mod sub {
    pub use inner::leaf;
}

#[macro_export]
macro_rules! wrapper {
    ($name:ident) => { $crate::sub::leaf!($name); };
}
",
        )];
        let scan = scan_graph(
            &[
                (
                    "lib.rs",
                    "facade::wrapper!(wrapped);\nfacade::sub::leaf!(direct_sub);\n",
                ),
                ("wrapped.rs", "pub unsafe fn a() {}\n"),
                ("direct_sub.rs", "pub unsafe fn b() {}\n"),
            ],
            &[1],
            &[
                GraphCrate {
                    name: "inner",
                    deps: &[],
                    files: &inner_files,
                },
                GraphCrate {
                    name: "facade",
                    deps: &[("inner", 0)],
                    files: &facade_files,
                },
            ],
        );
        assert_eq!(scan.unscanned, []);
        assert_eq!(scan.unsafe_code.functions, 2);
    }
}
