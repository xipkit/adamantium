mod attrs;
mod flatten;
mod macros;
mod modules;
mod resolve;
mod scope;
#[cfg(test)]
mod tests;
mod visitor;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, mem,
    path::{Component, Path, PathBuf},
};

use proc_macro2::{Span, TokenStream};
use serde::{Deserialize, Serialize};
use syn::{Attribute, Item, visit::Visit};
use toml_edit::DocumentMut;

use crate::paths;

use super::{
    cfg::{Cfg, Truth},
    disallowed::Disallowed,
    rules::Rules,
};

use self::{
    macros::{Found, Imported, MacroRules, Textual},
    resolve::Resolver,
    scope::{Namespace, Resolved, Scope},
    visitor::FileVisitor,
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
            let mut visitor = FileVisitor::new(
                &mut scanner,
                loaded.rel.clone(),
                loaded.module.clone(),
                loaded.mod_dir.clone(),
                loaded.path_dir.clone(),
                HashSet::new(),
            );
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
            let mut visitor = FileVisitor::new(
                &mut scanner,
                at.file,
                deferred.module,
                deferred.mod_dir,
                deferred.path_dir,
                deferred.hidden,
            );
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

struct Loaded {
    attrs: Vec<Attribute>,
    items: Vec<Item>,
    rel: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
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
    expanded: HashSet<DefinitionSite>,
    unexpanded: HashSet<String>,
    deferred: BTreeMap<DefinitionSite, Deferred>,
    sources: HashMap<String, String>,
    expansions: usize,
    prelude: Vec<Imported>,
    libraries: HashMap<usize, Scanner<'a>>,
    around: Vec<(Vec<String>, Scope)>,
    foreign: HashSet<String>,
    dollar_crates: HashMap<SpanStart, usize>,
    exports: Option<Vec<Export>>,
    crate_spans: Vec<Span>,
    macro_paths: HashMap<Vec<String>, Vec<MacroRules>>,
    last_pass: HashMap<Vec<String>, Scope>,
    path_calls: Vec<PathCall>,
    instances: HashMap<(DefinitionSite, usize), usize>,
    read_at: HashMap<DefinitionSite, usize>,
    hidden: HashMap<SpanStart, HashSet<String>>,
}

/// Where a span starts, by the name proc-macro2 gives its source file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SpanStart {
    file: String,
    line: usize,
    column: usize,
}

impl SpanStart {
    fn of(span: Span) -> Self {
        let start = span.start();
        Self {
            file: span.file(),
            line: start.line,
            column: start.column,
        }
    }
}

/// Where a macro definition starts, by its file's path in the report. The
/// order is by file, then line, then column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DefinitionSite {
    file: String,
    line: usize,
    column: usize,
}

impl DefinitionSite {
    fn new(file: String, span: Span) -> Self {
        let start = span.start();
        Self {
            file,
            line: start.line,
            column: start.column,
        }
    }
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

#[derive(Clone, Copy)]
struct Place<'p> {
    module: &'p [String],
    mod_dir: &'p Path,
    path_dir: &'p Path,
    rel: &'p str,
}

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
            scanner = scanner.next_pass();
        }
        scanner
    }

    /// A scanner for another reading of the crate. It keeps what the
    /// readings so far learned about other crates and about which macro
    /// definition is which, and resolves paths in the modules of the last one.
    fn next_pass(self) -> Self {
        Self {
            libraries: self.libraries,
            sources: self.sources,
            foreign: self.foreign,
            dollar_crates: self.dollar_crates,
            instances: self.instances,
            macro_paths: self.macro_paths,
            last_pass: self.modules,
            ..Self::new(self.crates, self.id, self.krate, self.disallowed)
        }
    }

    fn calls_settled(&mut self) -> bool {
        let calls = mem::take(&mut self.path_calls);
        calls
            .iter()
            .all(|call| self.call_macros(&call.module, &call.path, None).0 == call.found)
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

    fn relative(&self, path: &Path) -> String {
        let normal = paths::lexical(path);
        let path = normal.strip_prefix(self.krate.root).unwrap_or(&normal);
        path.components()
            .map(|component| match component {
                Component::RootDir => String::new(),
                component => component.as_os_str().to_string_lossy().into_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
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
