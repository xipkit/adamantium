//! Shared-state rewrites: local `Arc<Mutex<T>>` bindings become `Actor<T>`.
//!
//! A binding is rewritten only when every use of it, and of every clone of
//! it, has one of the forms the migration guide marks as mechanical: the
//! constructor, clones, a guard used within one statement, a copy of the state, and
//! taking the state back. Any other use leaves the binding and all of its
//! clones unchanged, and the caller reports why with a link to the guide.

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::Path,
    str::FromStr,
};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::{
    BinOp, Block, Expr, GenericArgument, Item, ItemUse, Lit, Local, Macro, Pat, PathArguments,
    Stmt, Type, UnOp, UseTree, ext::IdentExt, punctuated::Punctuated, spanned::Spanned,
    visit::Visit,
};

use super::Edit;

/// Why a binding was left for review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    /// A guard is bound, matched on or iterated, so it outlives its statement.
    AcrossStatements,
    /// The statement that takes the guard also awaits.
    AcrossAwait,
    /// The statement that takes the guard takes another lock too.
    TwoLocks,
    /// The statement that takes the guard returns, breaks or uses `?`.
    ControlFlow,
    /// The guard is taken inside another actor's `with` or `read` callback,
    /// where calling an actor panics.
    InsideActor,
    /// Any other use that the rewrite can't follow.
    Unfollowed(&'static str),
}

impl Reason {
    /// The migration guide's section for this case.
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::AcrossStatements => "a-guard-held-across-statements",
            Self::AcrossAwait => "a-lock-held-across-an-await",
            Self::TwoLocks | Self::InsideActor => "two-locks-at-once",
            Self::ControlFlow | Self::Unfollowed(_) => "shared-state",
        }
    }

    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::AcrossStatements => "a guard of it lives beyond one statement",
            Self::AcrossAwait => "a statement holding its guard also awaits",
            Self::TwoLocks => "a statement holding its guard takes another lock",
            Self::ControlFlow => "a statement holding its guard can return, break or use `?`",
            Self::InsideActor => "it is locked inside another actor's `with` or `read` callback",
            Self::Unfollowed(detail) => detail,
        }
    }
}

/// A binding left as it was.
pub(super) struct Refusal {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) reason: Reason,
}

/// The edits for one file, applied together, and the bindings left alone.
pub(super) struct Outcome {
    pub(super) edits: Vec<Edit>,
    pub(super) refusals: Vec<Refusal>,
}

const ACTOR: &str = "::adamantium::Actor";

/// A node whose text fix couldn't find, which only a parser bug would cause.
const UNLOCATED: Reason = Reason::Unfollowed("fix couldn't find its source text");

/// Methods that take a lock on std, Tokio, parking_lot and Adamantium types.
/// A statement that holds a guard and calls one of these holds two locks.
const LOCKS: &[&str] = &[
    "lock",
    "try_lock",
    "read",
    "write",
    "try_read",
    "try_write",
    "blocking_lock",
    "blocking_read",
    "blocking_write",
    "lock_owned",
    "read_owned",
    "write_owned",
    "with",
    "snapshot",
];

/// Plans the shared-state rewrites for one file. `mutexes` holds the byte
/// ranges that Clippy resolved to `std::sync::Mutex`.
pub(super) fn plan(file: &Path, source: &str, mutexes: &BTreeSet<(usize, usize)>) -> Outcome {
    let mut outcome = Outcome {
        edits: Vec::new(),
        refusals: Vec::new(),
    };
    let Ok(tree) = syn::parse_file(source) else {
        return outcome;
    };
    let text = Text {
        source,
        prefix: usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
            + tree.shebang.as_ref().map_or(0, String::len),
    };
    let mut names = Names::default();
    names.visit_file(&tree);
    let mut parsed = Parsed {
        text: &text,
        names: &names,
        macros: HashMap::new(),
    };
    parsed.visit_file(&tree);
    let macros = parsed.macros;
    let mut walker = Walker {
        text: &text,
        file,
        mutexes,
        names: &names,
        macros: &macros,
        vars: Vec::new(),
        scopes: Vec::new(),
        items: vec![ItemScope::new(&tree.items, true)],
        stack: Vec::new(),
        groups: Vec::new(),
        pending: Vec::new(),
        awaiting: Vec::new(),
    };
    walker.visit_file(&tree);
    let Walker {
        vars,
        mut groups,
        pending,
        awaiting,
        ..
    } = walker;
    for (group, _) in awaiting {
        if let Some(group) = groups.get_mut(group) {
            group.refuse(Reason::Unfollowed(
                "a guard is in a `let` fix didn't finish reading",
            ));
        }
    }
    for (group, var) in pending {
        if !vars.get(var).is_some_and(|var| var.used)
            && let Some(group) = groups.get_mut(group)
        {
            group.refuse(Reason::Unfollowed(
                "a value read under its guard is never used, so fix can't tell whether it borrows the state",
            ));
        }
    }
    refuse_overlaps(&mut groups);
    for group in groups {
        match group.refusal {
            Some(reason) => outcome.refusals.push(Refusal {
                line: group.line,
                name: group.name,
                reason,
            }),
            None => outcome.edits.extend(group.edits),
        }
    }
    if !outcome.edits.is_empty() {
        let imports = Imports::prune(&text, file, &tree, &outcome.edits);
        outcome.edits.extend(imports);
    }
    outcome.edits = coalesce(outcome.edits);
    outcome
}

/// Joins edits that touch, such as two adjacent `use` lines removed, since
/// the fix pipeline treats touching edits as conflicting.
fn coalesce(mut edits: Vec<Edit>) -> Vec<Edit> {
    edits.sort();
    let mut joined: Vec<Edit> = Vec::with_capacity(edits.len());
    for edit in edits {
        match joined.last_mut() {
            Some(last) if last.end == edit.start => {
                last.end = edit.end;
                last.replacement.push_str(&edit.replacement);
            }
            _ => joined.push(edit),
        }
    }
    joined
}

/// Rewrites in one file must not touch the same text: a guard inside the
/// constructor of another binding, say. Both bindings are left alone.
fn refuse_overlaps(groups: &mut [Group]) {
    let mut ranges: Vec<(usize, usize, usize)> = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.refusal.is_none())
        .flat_map(|(index, group)| {
            group
                .edits
                .iter()
                .map(move |edit| (edit.start, edit.end, index))
        })
        .collect();
    ranges.sort_unstable();
    let mut overlapping = BTreeSet::new();
    for (position, &(start, end, index)) in ranges.iter().enumerate() {
        for &(other_start, _, other) in ranges.iter().skip(position + 1) {
            if other_start >= end && other_start > start {
                break;
            }
            overlapping.insert(index);
            overlapping.insert(other);
        }
    }
    for index in overlapping {
        if let Some(group) = groups.get_mut(index) {
            group.refuse(Reason::Unfollowed(
                "rewriting it would overlap the rewrite of another binding",
            ));
        }
    }
}

struct Text<'a> {
    source: &'a str,
    /// The bytes before the text that `syn` parsed: a byte order mark and a
    /// shebang line.
    prefix: usize,
}

impl Text<'_> {
    fn range(&self, span: Span) -> Option<Range<usize>> {
        let range = span.byte_range();
        let range = (range.start + self.prefix)..(range.end + self.prefix);
        self.source.get(range.clone()).map(|_| range)
    }

    fn of<T: Spanned + ?Sized>(&self, node: &T) -> Option<Range<usize>> {
        self.range(node.span())
    }

    fn get(&self, range: Range<usize>) -> Option<&str> {
        self.source.get(range)
    }

    fn line(&self, span: Span) -> usize {
        span.start().line
    }
}

/// Names in the file that a pattern could match as a constant rather than
/// bind, and names that may shadow the standard macros.
#[derive(Default)]
struct Names {
    /// Constants, statics, structs and imports.
    constants: BTreeSet<String>,
    /// `macro_rules!` definitions and imports.
    macros: BTreeSet<String>,
    /// Type aliases for unsized types, such as `type Bytes = [u8];`.
    unsized_aliases: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Names {
    fn visit_item(&mut self, item: &'ast Item) {
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
                for (name, _) in ItemScope::bindings(item) {
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

/// The standard macros whose arguments are expressions, and the position of
/// their format string.
fn known_macro(name: &str) -> Option<usize> {
    match name {
        "format" | "print" | "println" | "eprint" | "eprintln" | "panic" => Some(0),
        "assert" | "debug_assert" | "write" | "writeln" => Some(1),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => Some(2),
        _ => None,
    }
}

/// A standard macro call whose arguments parsed as expressions.
struct MacroArgs {
    args: Vec<Expr>,
    format: usize,
    /// Whether the first argument is borrowed mutably, as the destination of
    /// `write!` and `writeln!` is.
    destination: bool,
    /// The first argument evaluated only on some paths: an assertion's
    /// message runs only when it fails, and a debug assertion only in debug
    /// builds.
    conditional: usize,
}

impl MacroArgs {
    /// The value of a named format argument such as `width = 4`, which only
    /// follows the format string. Any other `=` is an assignment to scan.
    fn named<'e>(&self, position: usize, arg: &'e Expr) -> Option<&'e Expr> {
        match arg {
            Expr::Assign(named)
                if position > self.format
                    && matches!(&*named.left, Expr::Path(path) if path.path.get_ident().is_some()) =>
            {
                Some(&named.right)
            }
            _ => None,
        }
    }
}

/// Parses the arguments of every standard macro call, nested ones included,
/// keyed by where the call starts.
struct Parsed<'t> {
    text: &'t Text<'t>,
    names: &'t Names,
    macros: HashMap<usize, MacroArgs>,
}

impl<'ast> Visit<'ast> for Parsed<'_> {
    fn visit_macro(&mut self, mac: &'ast Macro) {
        let Some(name) = mac
            .path
            .get_ident()
            .map(ToString::to_string)
            .filter(|name| mac.path.leading_colon.is_none() && !self.names.macros.contains(name))
        else {
            return;
        };
        let Some(format) = known_macro(&name) else {
            return;
        };
        let Some(start) = self.text.of(mac).map(|range| range.start) else {
            return;
        };
        let Ok(args) = mac.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated)
        else {
            return;
        };
        let args: Vec<Expr> = args.into_iter().collect();
        for arg in &args {
            self.visit_expr(arg);
        }
        self.macros.insert(
            start,
            MacroArgs {
                args,
                format,
                destination: name == "write" || name == "writeln",
                conditional: if name.starts_with("debug_assert") {
                    0
                } else if name.starts_with("assert") {
                    format
                } else {
                    usize::MAX
                },
            },
        );
    }
}

/// The names in an `std::fmt` format string that capture variables, such as
/// `x` and `width` in `"{x:width$}"`.
fn format_captures(format: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let mut spec = String::new();
        for c in chars.by_ref() {
            if c == '}' {
                break;
            }
            spec.push(c);
        }
        let (argument, options) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
        if is_identifier(argument.trim()) {
            names.push(argument.trim().to_owned());
        }
        for part in options.split('$').rev().skip(1) {
            let name: String = part
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if is_identifier(&name) {
                names.push(name);
            }
        }
    }
    names
}

fn is_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && name != "_"
}

/// What a name means in one item scope.
#[derive(Clone, PartialEq, Eq)]
enum Binding {
    /// A `use` or `extern crate`, with the path it names.
    Import(Vec<String>),
    /// An item defined here.
    Item,
    /// A name whose meaning depends on the configuration, or that two
    /// declarations claim.
    Ambiguous,
}

/// The items and imports of one module or block.
struct ItemScope {
    names: HashMap<String, Binding>,
    globs: Vec<Vec<String>>,
    module: bool,
}

impl ItemScope {
    fn new<'i>(items: impl IntoIterator<Item = &'i Item>, module: bool) -> Self {
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
                own.import(&item.tree, &mut Vec::new());
            } else {
                own.names.extend(Self::bindings(item));
            }
            scope.globs.extend(own.globs);
            // A `#[cfg]` item may not exist in the build fix checked.
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

    /// The names an item other than `use` defines.
    fn bindings(item: &Item) -> Vec<(String, Binding)> {
        let item_name = |ident: &syn::Ident| vec![(ident.to_string(), Binding::Item)];
        match item {
            Item::Const(item) => item_name(&item.ident),
            Item::Enum(item) => item_name(&item.ident),
            Item::ExternCrate(item) => {
                let name = item.rename.as_ref().map_or(&item.ident, |(_, name)| name);
                vec![(
                    name.to_string(),
                    Binding::Import(vec![item.ident.to_string()]),
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

fn item_attributes(item: &Item) -> &[syn::Attribute] {
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

/// What a single name resolves to among the file's items.
enum Resolved {
    Path(Vec<String>),
    Item,
    /// Not declared in the file: the extern or standard prelude.
    Prelude,
    /// A glob import might declare it.
    Ambiguous,
}

/// A local variable.
struct Var {
    /// The name without `r#`, under which `m` and `r#m` are one variable.
    name: String,
    /// The name as the declaration spells it, for generated code.
    spelling: String,
    /// The shared-state binding this variable holds, if it holds one.
    group: Option<usize>,
    used: bool,
}

/// One `Arc<Mutex<T>>` and every variable that holds a clone of it.
struct Group {
    name: String,
    line: usize,
    refusal: Option<Reason>,
    edits: Vec<Edit>,
}

impl Group {
    fn refuse(&mut self, reason: Reason) {
        self.refusal.get_or_insert(reason);
    }
}

struct Scope {
    vars: Vec<usize>,
    /// An item boundary: locals of enclosing functions aren't visible.
    barrier: bool,
}

/// How a block ends a guard's temporary scope.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// A plain block, whose final expression's temporaries outlive it.
    Plain,
    /// A body that drops its temporaries before its value is used: a
    /// function, closure, async block, loop or `if` branch.
    Body,
}

#[derive(Clone, Copy)]
enum Node<'a> {
    Expr(&'a Expr),
    Stmt(&'a Stmt),
    Block(&'a Block, BlockKind),
    /// The arguments of a standard macro.
    Macro(&'a MacroArgs),
}

/// The expression whose evaluation a guard's temporary spans.
#[derive(Clone, Copy)]
enum Root<'a> {
    /// A statement whose value is dropped.
    Statement(Full<'a>, bool),
    /// A `let` initializer.
    Let(&'a Local, &'a Expr),
    /// An expression whose value leaves a temporary scope: a body's final
    /// expression, a `return` value, a match arm, a lazy boolean operand.
    Value(&'a Expr),
    /// An `if` or `while` condition or a match guard.
    Condition(&'a Expr),
}

#[derive(Clone, Copy)]
enum Full<'a> {
    Expr(&'a Expr),
    Macro(&'a Macro),
}

impl<'a> Root<'a> {
    fn full(self) -> Full<'a> {
        match self {
            Self::Statement(full, _) => full,
            Self::Let(_, expr) | Self::Value(expr) | Self::Condition(expr) => Full::Expr(expr),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    With,
}

impl Mode {
    fn method(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::With => "with",
        }
    }
}

struct Walker<'a> {
    text: &'a Text<'a>,
    file: &'a Path,
    mutexes: &'a BTreeSet<(usize, usize)>,
    names: &'a Names,
    macros: &'a HashMap<usize, MacroArgs>,
    vars: Vec<Var>,
    scopes: Vec<Scope>,
    items: Vec<ItemScope>,
    stack: Vec<Node<'a>>,
    groups: Vec<Group>,
    /// `let` initializers that read under a guard, kept only if the variable
    /// they declare is used later: (group, variable).
    pending: Vec<(usize, usize)>,
    /// The same, while the `let` is still being read.
    awaiting: Vec<(usize, &'a Local)>,
}

impl<'a> Walker<'a> {
    fn edit(&self, range: Range<usize>, replacement: String) -> Edit {
        Edit {
            file: self.file.to_path_buf(),
            start: range.start,
            end: range.end,
            replacement,
        }
    }

    fn refuse(&mut self, var: usize, reason: Reason) {
        if let Some(group) = self.vars.get(var).and_then(|var| var.group)
            && let Some(group) = self.groups.get_mut(group)
        {
            group.refuse(reason);
        }
    }

    fn group_of(&self, var: usize) -> Option<usize> {
        self.vars.get(var).and_then(|var| var.group)
    }

    fn add_edit(&mut self, var: usize, edit: Edit) {
        if let Some(group) = self
            .group_of(var)
            .and_then(|group| self.groups.get_mut(group))
        {
            group.edits.push(edit);
        }
    }

    fn declare(&mut self, name: String, group: Option<usize>) -> usize {
        let index = self.vars.len();
        self.vars.push(Var {
            spelling: name.clone(),
            name,
            group,
            used: false,
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.vars.push(index);
        }
        index
    }

    fn declare_pattern(&mut self, pat: &Pat) {
        let mut names = Vec::new();
        pattern_names(pat, &mut names);
        for name in names {
            self.declare(name, None);
        }
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            for &var in scope.vars.iter().rev() {
                if self.vars.get(var).is_some_and(|var| var.name == name) {
                    return Some(var);
                }
            }
            if scope.barrier {
                break;
            }
        }
        None
    }

    /// Records a use of a variable by name, refusing a shared-state binding
    /// used where fix can't follow it.
    fn mention(&mut self, name: &str, reason: Reason) {
        if let Some(var) = self.lookup(name) {
            if let Some(var) = self.vars.get_mut(var) {
                var.used = true;
            }
            self.refuse(var, reason);
        }
    }

    /// The variable that a bare single-name path refers to.
    fn local(&self, expr: &Expr) -> Option<usize> {
        let Expr::Path(path) = expr else {
            return None;
        };
        if path.qself.is_some() || !path.attrs.is_empty() {
            return None;
        }
        self.lookup(&bare(path.path.get_ident()?))
    }

    /// A tracked variable named by `expr`.
    fn tracked(&self, expr: &Expr) -> Option<usize> {
        self.local(expr).filter(|&var| self.group_of(var).is_some())
    }

    fn resolve_name(&self, name: &str) -> Resolved {
        let mut scopes = self.items.iter().rev();
        while let Some(scope) = scopes.next() {
            if let Some(binding) = scope.names.get(name) {
                return match binding {
                    Binding::Import(path) => Resolved::Path(path.clone()),
                    Binding::Item => Resolved::Item,
                    Binding::Ambiguous => Resolved::Ambiguous,
                };
            }
            if scope.module {
                // `use super::*` in an inline module sees its parent's names.
                let parent = scopes.clone().find(|scope| scope.module);
                let only_super = scope.globs.iter().all(|glob| glob == &["super"]);
                if !scope.globs.is_empty() {
                    return match parent {
                        Some(parent) if only_super && parent.globs.is_empty() => {
                            match parent.names.get(name) {
                                Some(Binding::Import(path)) => Resolved::Path(path.clone()),
                                Some(Binding::Item) => Resolved::Item,
                                Some(Binding::Ambiguous) => Resolved::Ambiguous,
                                None => Resolved::Prelude,
                            }
                        }
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

    /// The full path that `segments` names, if every segment is a plain name.
    fn resolve_path(&self, path: &syn::Path, segments: usize) -> Option<Vec<String>> {
        let names: Vec<String> = path
            .segments
            .iter()
            .take(segments)
            .map(|segment| {
                matches!(segment.arguments, PathArguments::None).then(|| segment.ident.to_string())
            })
            .collect::<Option<_>>()?;
        if path.leading_colon.is_some() {
            return Some(names);
        }
        let (first, rest) = names.split_first()?;
        let mut resolved = match self.resolve_name(first) {
            Resolved::Path(path) => path,
            Resolved::Prelude => vec![first.clone()],
            Resolved::Item | Resolved::Ambiguous => return None,
        };
        resolved.extend(rest.iter().cloned());
        Some(resolved)
    }

    /// Whether `path` without its last `skip` segments is `std::sync::Arc`.
    fn is_arc(&self, path: &syn::Path, skip: usize) -> bool {
        let Some(count) = path.segments.len().checked_sub(skip) else {
            return false;
        };
        self.resolve_path(path, count).is_some_and(|resolved| {
            matches!(
                resolved
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                ["std" | "alloc", "sync", "Arc"]
            )
        })
    }

    /// Whether Clippy resolved the text at `range` to `std::sync::Mutex`.
    fn is_mutex(&self, range: Option<Range<usize>>) -> bool {
        range.is_some_and(|range| self.mutexes.contains(&(range.start, range.end)))
    }

    /// The text of `Mutex` in a path such as `Mutex::new`: every segment but
    /// the last.
    fn type_prefix(&self, path: &syn::Path) -> Option<Range<usize>> {
        let count = path.segments.len().checked_sub(1)?;
        let first = match &path.leading_colon {
            Some(colon) => self.text.range(*colon.spans.first()?)?,
            None => self.text.range(path.segments.first()?.ident.span())?,
        };
        let last = self.text.range(
            path.segments
                .iter()
                .nth(count.checked_sub(1)?)?
                .ident
                .span(),
        )?;
        Some(first.start..last.end)
    }

    /// `Arc<Mutex<T>>`, as `::adamantium::Actor<T>`.
    fn actor_type(&self, ty: &Type) -> Option<String> {
        let Type::Path(arc) = ty else {
            return None;
        };
        if arc.qself.is_some() {
            return None;
        }
        let PathArguments::AngleBracketed(args) = &arc.path.segments.last()?.arguments else {
            return None;
        };
        let mut prefix = arc.path.clone();
        prefix.segments.last_mut()?.arguments = PathArguments::None;
        if args.args.len() != 1 || !self.is_arc(&prefix, 0) {
            return None;
        }
        let Some(GenericArgument::Type(Type::Path(mutex))) = args.args.first() else {
            return None;
        };
        if mutex.qself.is_some() || !self.is_mutex(self.text.of(mutex)) {
            return None;
        }
        let PathArguments::AngleBracketed(inner) = &mutex.path.segments.last()?.arguments else {
            return None;
        };
        let Some(GenericArgument::Type(state)) = inner.args.first() else {
            return None;
        };
        // A mutex can hold a slice or trait object; an actor can't.
        if inner.args.len() != 1 || is_unsized(state, &self.names.unsized_aliases) {
            return None;
        }
        Some(format!("{ACTOR}<{}>", self.text.get(self.text.of(state)?)?))
    }

    /// `Arc::new(Mutex::new(x))` or `Arc::new(Mutex::default())`, as an
    /// `Actor` constructor.
    fn constructor(&self, expr: &Expr) -> Option<String> {
        let Expr::Call(arc) = expr else {
            return None;
        };
        let Expr::Path(arc_new) = &*arc.func else {
            return None;
        };
        if arc_new.qself.is_some()
            || arc.args.len() != 1
            || arc_new.path.segments.last()?.ident != "new"
            || !self.is_arc(&arc_new.path, 1)
        {
            return None;
        }
        let Some(Expr::Call(mutex)) = arc.args.first() else {
            return None;
        };
        let Expr::Path(mutex_new) = &*mutex.func else {
            return None;
        };
        let last = mutex_new.path.segments.last()?;
        if mutex_new.qself.is_some()
            || mutex_new
                .path
                .segments
                .iter()
                .any(|segment| !matches!(segment.arguments, PathArguments::None))
            || !self.is_mutex(self.type_prefix(&mutex_new.path))
        {
            return None;
        }
        match (last.ident.to_string().as_str(), mutex.args.first()) {
            ("new", Some(state)) if mutex.args.len() == 1 => Some(format!(
                "{ACTOR}::new({})",
                self.text.get(self.text.of(state)?)?
            )),
            ("default", None) => Some(format!("{ACTOR}::default()")),
            _ => None,
        }
    }

    /// The variable cloned or moved by `Arc::clone(&m)`, `m.clone()` or `m`,
    /// with the replacement for the first form.
    fn clone_of(&self, expr: &Expr) -> Option<(usize, Option<String>)> {
        match expr {
            Expr::Path(_) => self.tracked(expr).map(|var| (var, None)),
            Expr::MethodCall(call)
                if call.method == "clone" && call.args.is_empty() && call.turbofish.is_none() =>
            {
                self.tracked(&call.receiver).map(|var| (var, None))
            }
            Expr::Call(call) => {
                let Expr::Path(func) = &*call.func else {
                    return None;
                };
                let Some(Expr::Reference(reference)) = call.args.first() else {
                    return None;
                };
                if func.qself.is_some()
                    || call.args.len() != 1
                    || reference.mutability.is_some()
                    || func.path.segments.last()?.ident != "clone"
                    || !self.is_arc(&func.path, 1)
                {
                    return None;
                }
                let var = self.tracked(&reference.expr)?;
                let name = &self.vars.get(var)?.spelling;
                Some((var, Some(format!("{name}.clone()"))))
            }
            _ => None,
        }
    }

    fn local_statement(&mut self, local: &'a Local) {
        let (pat, ty) = match &local.pat {
            Pat::Type(typed) => (&*typed.pat, Some(&*typed.ty)),
            pat => (pat, None),
        };
        let ident = match pat {
            Pat::Ident(ident) if ident.by_ref.is_none() && ident.subpat.is_none() => Some(ident),
            _ => None,
        };
        let init = local
            .init
            .as_ref()
            .filter(|init| init.diverge.is_none())
            .map(|init| &*init.expr);
        let line = self.text.line(local.let_token.span);
        if let (Some(ident), Some(init)) = (ident, init) {
            let name = bare(&ident.ident);
            let spelling = ident.ident.to_string();
            let ty_edit = match ty {
                None => Some(None),
                Some(ty) => self
                    .actor_type(ty)
                    .and_then(|actor| Some(Some(self.edit(self.text.of(ty)?, actor)))),
            };
            if let Some(constructor) = self.constructor(init)
                && let Some(ty_edit) = ty_edit.clone()
                && let Some(range) = self.text.of(init)
            {
                self.visit_expr(init);
                let group = self.groups.len();
                let mut group_state = Group {
                    name: name.clone(),
                    line,
                    refusal: None,
                    edits: vec![self.edit(range, constructor)],
                };
                group_state.edits.extend(ty_edit);
                if !local.attrs.is_empty() {
                    group_state.refuse(Reason::Unfollowed("its declaration has attributes"));
                }
                if self.names.constants.contains(&name) {
                    group_state.refuse(Reason::Unfollowed(
                        "a constant, static, struct or import in the file has its name",
                    ));
                }
                self.groups.push(group_state);
                let var = self.declare(name, Some(group));
                if let Some(entry) = self.vars.get_mut(var) {
                    entry.spelling = spelling;
                }
                self.settle(local, Some(var));
                return;
            }
            if let Some((source, replacement)) = self.clone_of(init) {
                if let Some(var) = self.vars.get_mut(source) {
                    var.used = true;
                }
                let group = self.group_of(source);
                match (ty_edit, self.text.of(init)) {
                    (Some(ty_edit), Some(range)) => {
                        if let Some(replacement) = replacement {
                            let edit = self.edit(range, replacement);
                            self.add_edit(source, edit);
                        }
                        if let Some(edit) = ty_edit {
                            self.add_edit(source, edit);
                        }
                    }
                    _ => self.refuse(
                        source,
                        Reason::Unfollowed("a clone of it is declared with another type"),
                    ),
                }
                if !local.attrs.is_empty() {
                    self.refuse(source, Reason::Unfollowed("a clone of it has attributes"));
                }
                if self.names.constants.contains(&name) {
                    self.refuse(
                        source,
                        Reason::Unfollowed(
                            "a constant, static, struct or import in the file has its name",
                        ),
                    );
                }
                let var = self.declare(name, group);
                if let Some(entry) = self.vars.get_mut(var) {
                    entry.spelling = spelling;
                }
                self.settle(local, Some(var));
                return;
            }
        }
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        let mut names = Vec::new();
        pattern_names(&local.pat, &mut names);
        let mut declared = None;
        for name in names {
            // A conditional `let` may shadow a binding in one configuration
            // and not in another.
            if !local.attrs.is_empty()
                && let Some(var) = self.lookup(&name)
            {
                self.refuse(
                    var,
                    Reason::Unfollowed("a conditional declaration shadows it"),
                );
            }
            declared = Some(self.declare(name, None));
        }
        self.settle(local, ident.and(declared));
    }

    /// Resolves the checks waiting on `local`: a value read under a guard
    /// must go to a single variable that is used later.
    fn settle(&mut self, local: &'a Local, var: Option<usize>) {
        let waiting: Vec<usize> = self
            .awaiting
            .extract_if(.., |(_, awaiting)| std::ptr::eq(*awaiting, local))
            .map(|(group, _)| group)
            .collect();
        for group in waiting {
            match var {
                Some(var) => self.pending.push((group, var)),
                None => {
                    if let Some(group) = self.groups.get_mut(group) {
                        group.refuse(Reason::Unfollowed(
                            "a value read under its guard is bound to a pattern",
                        ));
                    }
                }
            }
        }
    }
}

/// Whether `ty` is written as an unsized type: a slice, `str`, a trait
/// object, or one of `aliases`.
fn is_unsized(ty: &Type, aliases: &BTreeSet<String>) -> bool {
    match ty {
        Type::Slice(_) | Type::TraitObject(_) => true,
        Type::Paren(inner) => is_unsized(&inner.elem, aliases),
        Type::Group(inner) => is_unsized(&inner.elem, aliases),
        Type::Path(path) => {
            path.qself.is_none()
                && path
                    .path
                    .get_ident()
                    .is_some_and(|ident| ident == "str" || aliases.contains(&ident.to_string()))
        }
        _ => false,
    }
}

/// The variables a pattern binds.
fn pattern_names(pat: &Pat, names: &mut Vec<String>) {
    match pat {
        Pat::Ident(ident) => {
            names.push(bare(&ident.ident));
            if let Some((_, subpat)) = &ident.subpat {
                pattern_names(subpat, names);
            }
        }
        Pat::Guard(guard) => pattern_names(&guard.pat, names),
        Pat::Or(or) => {
            if let Some(first) = or.cases.first() {
                pattern_names(first, names);
            }
        }
        Pat::Paren(paren) => pattern_names(&paren.pat, names),
        Pat::Reference(reference) => pattern_names(&reference.pat, names),
        Pat::Slice(slice) => slice.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::Struct(fields) => fields
            .fields
            .iter()
            .for_each(|field| pattern_names(&field.pat, names)),
        Pat::Tuple(tuple) => tuple.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::TupleStruct(tuple) => tuple.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::Type(typed) => pattern_names(&typed.pat, names),
        _ => {}
    }
}

/// How a guard's statement becomes an actor call.
struct Plan<'a> {
    root: Root<'a>,
    /// A smaller expression to read instead of the whole root: one whose
    /// value can't borrow the state.
    narrow: Option<&'a Expr>,
    mode: Mode,
    /// What replaces the guard: the callback's parameter, or for a method
    /// receiver, the state it points to.
    receiver: bool,
}

/// Whether `child` is the node `expr`.
fn is(expr: &Expr, child: &Expr) -> bool {
    std::ptr::eq(expr, child)
}

fn is_comparison(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq(_) | BinOp::Ne(_) | BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_)
    )
}

fn is_assignment(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::AddAssign(_)
            | BinOp::SubAssign(_)
            | BinOp::MulAssign(_)
            | BinOp::DivAssign(_)
            | BinOp::RemAssign(_)
            | BinOp::BitXorAssign(_)
            | BinOp::BitAndAssign(_)
            | BinOp::BitOrAssign(_)
            | BinOp::ShlAssign(_)
            | BinOp::ShrAssign(_)
    )
}

/// Whether `parent` copies its operand `child` out by value. A place read
/// through a guard by value must be `Copy`, so the value read can't borrow
/// the state. Only operator operands and indexes count: at a coercion site,
/// such as a call argument or a field, a `&mut` place is reborrowed rather
/// than copied, and the reborrow needs the guard.
fn copies_out(parent: &Expr, child: &Expr) -> bool {
    match parent {
        Expr::Binary(binary) => !is_comparison(&binary.op) && !is_assignment(&binary.op),
        Expr::Unary(unary) => !matches!(unary.op, UnOp::Deref(_)),
        Expr::Index(index) => is(&index.index, child),
        _ => false,
    }
}

impl<'a> Walker<'a> {
    /// The variable whose guard `expr` is: `m.lock().unwrap()` or
    /// `m.lock().expect("...")`.
    fn guard_var(&self, expr: &Expr) -> Option<usize> {
        let Expr::MethodCall(unwrap) = expr else {
            return None;
        };
        let Expr::MethodCall(lock) = &*unwrap.receiver else {
            return None;
        };
        if !is_unwrap(unwrap)
            || lock.method != "lock"
            || !lock.args.is_empty()
            || lock.turbofish.is_some()
        {
            return None;
        }
        self.tracked(&lock.receiver)
    }

    /// `Arc::try_unwrap(m).unwrap().into_inner().unwrap()`, or the same with
    /// `Arc::into_inner`, as `m.into_inner()` followed by the first unwrap.
    fn unwrapped(&self, expr: &Expr) -> Option<(usize, String)> {
        let Expr::MethodCall(outer) = expr else {
            return None;
        };
        let Expr::MethodCall(into_inner) = &*outer.receiver else {
            return None;
        };
        let Expr::MethodCall(first) = &*into_inner.receiver else {
            return None;
        };
        let Expr::Call(call) = &*first.receiver else {
            return None;
        };
        let Expr::Path(func) = &*call.func else {
            return None;
        };
        if !is_unwrap(outer)
            || !is_unwrap(first)
            || into_inner.method != "into_inner"
            || !into_inner.args.is_empty()
            || into_inner.turbofish.is_some()
            || func.qself.is_some()
            || call.args.len() != 1
            || !self.is_arc(&func.path, 1)
        {
            return None;
        }
        let ok = match func.path.segments.last()?.ident.to_string().as_str() {
            "try_unwrap" => "",
            "into_inner" => ".ok()",
            _ => return None,
        };
        let var = self.tracked(call.args.first()?)?;
        let start = self.text.range(first.method.span())?.start;
        let end = self.text.range(first.paren_token.span.close())?.end;
        let unwrap = self.text.get(start..end)?;
        let name = &self.vars.get(var)?.spelling;
        Some((var, format!("{name}.into_inner(){ok}.{unwrap}")))
    }

    /// The variable dropped by `drop(m)`.
    fn dropped(&self, expr: &Expr) -> Option<usize> {
        let Expr::Call(call) = expr else {
            return None;
        };
        let Expr::Path(func) = &*call.func else {
            return None;
        };
        if func.qself.is_some() || call.args.len() != 1 {
            return None;
        }
        let is_drop = match func.path.get_ident() {
            Some(name) => {
                name == "drop"
                    && self.lookup("drop").is_none()
                    && matches!(self.resolve_name("drop"), Resolved::Prelude)
            }
            None => self
                .resolve_path(&func.path, func.path.segments.len())
                .is_some_and(|path| {
                    matches!(
                        path.iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .as_slice(),
                        ["std" | "core", "mem", "drop"]
                    )
                }),
        };
        is_drop.then(|| self.tracked(call.args.first()?)).flatten()
    }

    /// Finds the statement a guard lives for, or why it lives longer. The
    /// guard is on top of the stack.
    fn root(&self) -> Result<Root<'a>, Reason> {
        let mut index = self.stack.len().checked_sub(1).ok_or(UNLOCATED)?;
        let Some(Node::Expr(mut child)) = self.stack.get(index).copied() else {
            return Err(Reason::Unfollowed("a guard is outside any statement"));
        };
        // Set when a plain block's final expression passes its value up.
        let mut from_block = false;
        while let Some(next) = index.checked_sub(1) {
            index = next;
            let Some(node) = self.stack.get(index).copied() else {
                break;
            };
            match node {
                Node::Expr(parent) => {
                    if !from_block {
                        match parent {
                            Expr::Closure(closure) if is(&closure.body, child) => {
                                return Ok(Root::Value(child));
                            }
                            Expr::If(branch) if is(&branch.cond, child) => {
                                return Ok(Root::Condition(child));
                            }
                            Expr::While(lp) if is(&lp.cond, child) => {
                                return Ok(Root::Condition(child));
                            }
                            Expr::Let(_) | Expr::ForLoop(_) => {
                                return Err(Reason::AcrossStatements);
                            }
                            Expr::Match(m) if is(&m.expr, child) => {
                                return Err(Reason::AcrossStatements);
                            }
                            Expr::Match(m) => {
                                for arm in &m.arms {
                                    if is(&arm.body, child) {
                                        return Ok(Root::Value(child));
                                    }
                                    if let Pat::Guard(guard) = &arm.pat
                                        && is(&guard.guard, child)
                                    {
                                        return Ok(Root::Condition(child));
                                    }
                                }
                            }
                            // Each operand of `&&` and `||` drops its own
                            // temporaries.
                            Expr::Binary(binary)
                                if matches!(binary.op, BinOp::And(_) | BinOp::Or(_)) =>
                            {
                                return Ok(Root::Value(child));
                            }
                            Expr::Await(_) => return Err(Reason::AcrossAwait),
                            Expr::Return(_) => return Ok(Root::Value(child)),
                            _ => {}
                        }
                    }
                    from_block = false;
                    child = parent;
                }
                Node::Stmt(stmt) => match stmt {
                    Stmt::Local(local) => {
                        return match &local.init {
                            Some(init) if is(&init.expr, child) && init.diverge.is_none() => {
                                Ok(Root::Let(local, child))
                            }
                            _ => Err(Reason::Unfollowed("a guard is in a `let`-`else`")),
                        };
                    }
                    Stmt::Expr(_, Some(_)) => return Ok(Root::Statement(Full::Expr(child), true)),
                    Stmt::Expr(_, None) => {
                        let Some(Node::Block(block, kind)) = index
                            .checked_sub(1)
                            .and_then(|below| self.stack.get(below))
                            .copied()
                        else {
                            return Err(Reason::Unfollowed("a guard is outside any block"));
                        };
                        let tail = block
                            .stmts
                            .last()
                            .is_some_and(|last| std::ptr::eq(last, stmt));
                        if !tail {
                            return Ok(Root::Statement(Full::Expr(child), false));
                        }
                        if kind == BlockKind::Body {
                            return Ok(Root::Value(child));
                        }
                        // A plain block's value keeps its temporaries alive in
                        // the enclosing expression.
                        index = index.saturating_sub(1);
                        from_block = true;
                    }
                    Stmt::Macro(mac) => {
                        return Ok(Root::Statement(
                            Full::Macro(&mac.mac),
                            mac.semi_token.is_some(),
                        ));
                    }
                    Stmt::Item(_) => return Err(Reason::Unfollowed("a guard is in an item")),
                },
                Node::Block(..) | Node::Macro(_) => {}
            }
        }
        Err(Reason::Unfollowed("a guard is outside any statement"))
    }

    fn plan(&self, guard: &'a Expr) -> Result<Plan<'a>, Reason> {
        let top = self.stack.len().checked_sub(1).ok_or(UNLOCATED)?;
        // Climb the projections of the guard: `*guard`, `guard.field`,
        // `guard[i]`.
        let mut chain = guard;
        let mut index = top;
        // Parentheses alone leave the guard itself.
        let mut projected = false;
        let parent = loop {
            let below = index.checked_sub(1).and_then(|below| self.stack.get(below));
            match below.copied() {
                Some(Node::Expr(parent))
                    if match parent {
                        Expr::Field(field) => is(&field.base, chain),
                        Expr::Index(at) => is(&at.expr, chain),
                        Expr::Unary(unary) => matches!(unary.op, UnOp::Deref(_)),
                        Expr::Paren(_) => true,
                        _ => false,
                    } =>
                {
                    projected |= !matches!(parent, Expr::Paren(_));
                    chain = parent;
                    index = index.saturating_sub(1);
                }
                other => break other,
            }
        };
        if !projected {
            match parent {
                Some(Node::Expr(Expr::MethodCall(call))) if is(&call.receiver, chain) => {}
                Some(Node::Stmt(Stmt::Local(_))) => return Err(Reason::AcrossStatements),
                _ => return Err(Reason::Unfollowed("a guard is used as a value")),
            }
        }
        let root = self.root()?;
        let narrow = match parent {
            Some(Node::Expr(expr @ Expr::Binary(binary)))
                if projected && is_comparison(&binary.op) =>
            {
                Some(expr)
            }
            Some(Node::Expr(expr)) if projected && copies_out(expr, chain) => Some(chain),
            _ => None,
        };
        let read = projected
            && match parent {
                Some(Node::Expr(expr)) => {
                    copies_out(expr, chain)
                        || matches!(expr, Expr::Binary(binary) if is_comparison(&binary.op))
                        || matches!(expr, Expr::Reference(reference) if reference.mutability.is_none())
                        || matches!(
                            expr,
                            Expr::Return(_) | Expr::If(_) | Expr::While(_) | Expr::Closure(_)
                        )
                }
                // Format and assertion macros take their arguments by
                // reference, except a `write!` destination.
                Some(Node::Macro(args)) => {
                    !(args.destination && args.args.first().is_some_and(|first| is(first, chain)))
                }
                Some(Node::Stmt(Stmt::Local(_))) => true,
                Some(Node::Stmt(Stmt::Expr(_, None))) => true,
                _ => false,
            };
        Ok(Plan {
            root,
            narrow,
            mode: if read { Mode::Read } else { Mode::With },
            receiver: !projected,
        })
    }

    fn guard(&mut self, var: usize, guard: &'a Expr) {
        if let Some(entry) = self.vars.get_mut(var) {
            entry.used = true;
        }
        match self.guard_edit(var, guard) {
            Ok(edit) => self.add_edit(var, edit),
            Err(reason) => self.refuse(var, reason),
        }
    }

    fn guard_edit(&mut self, var: usize, guard: &'a Expr) -> Result<Edit, Reason> {
        if self.inside_actor() {
            return Err(Reason::InsideActor);
        }
        let plan = self.plan(guard)?;
        let (bare_name, name) = self
            .vars
            .get(var)
            .map(|var| (var.name.clone(), var.spelling.clone()))
            .ok_or(UNLOCATED)?;
        let guard_range = self.text.of(guard).ok_or(UNLOCATED)?;
        let full = plan.root.full();
        let mut scan = Scan::new(self, guard);
        scan.full(full);
        if scan.awaits {
            return Err(Reason::AcrossAwait);
        }
        if scan.locks {
            return Err(Reason::TwoLocks);
        }
        let inside = match plan.narrow {
            Some(expr) => {
                let mut scan = Scan::new(self, guard);
                scan.visit_expr(expr);
                scan
            }
            None => scan,
        };
        if inside.leaves {
            return Err(Reason::ControlFlow);
        }
        if let Some(reason) = inside.unfollowed {
            return Err(Reason::Unfollowed(reason));
        }
        // Method lookup on a guard starts at the guard and reaches the state
        // through `Deref`; on the callback's `&mut T` it would start at the
        // reference and could pick another trait's method. Starting at `*m`
        // keeps the state's lookup.
        let state = if plan.receiver {
            format!("(*{name})")
        } else {
            name.clone()
        };
        // The closure's parameter takes the binding's name, so the code that
        // moves into the closure must not name the binding anywhere else.
        let substitute = |range: Range<usize>| -> Result<String, Reason> {
            let region = self.text.get(range.clone()).ok_or(UNLOCATED)?;
            if names_in(region, &bare_name) != Some(1) {
                return Err(Reason::Unfollowed(
                    "a statement holding its guard names it again",
                ));
            }
            let before = self.text.get(range.start..guard_range.start);
            let after = self.text.get(guard_range.end..range.end);
            match (before, after) {
                (Some(before), Some(after)) => Ok(format!("{before}{state}{after}")),
                _ => Err(UNLOCATED),
            }
        };
        if let Some(expr) = plan.narrow {
            let range = self.text.of(expr).ok_or(UNLOCATED)?;
            let replacement = format!("{name}.read(|{name}| {})", substitute(range.clone())?);
            return Ok(self.edit(range, replacement));
        }
        let method = plan.mode.method();
        let (range, replacement) = match plan.root {
            Root::Statement(full, semi) => {
                let (range, assignment, attributed) = match full {
                    Full::Expr(expr) => (
                        self.text.of(expr),
                        matches!(expr, Expr::Assign(_))
                            || matches!(expr, Expr::Binary(binary) if is_assignment(&binary.op)),
                        self.attributed(expr),
                    ),
                    Full::Macro(mac) => (self.text.of(mac), false, false),
                };
                let range = range.ok_or(UNLOCATED)?;
                if attributed || self.statement_attributes() {
                    return Err(Reason::Unfollowed(
                        "a statement holding its guard has attributes",
                    ));
                }
                if let Full::Expr(Expr::Assign(assign)) = full
                    && !self.text.of(&assign.left).is_some_and(|left| {
                        left.start <= guard_range.start && guard_range.end <= left.end
                    })
                {
                    return Err(Reason::Unfollowed(
                        "a value read under its guard is assigned to a variable",
                    ));
                }
                let body = substitute(range.clone())?;
                let call = if assignment {
                    format!("{name}.{method}(|{name}| {body})")
                } else {
                    format!("{name}.{method}(|{name}| {{ {body}; }})")
                };
                (range, if semi { call } else { format!("{call};") })
            }
            Root::Let(local, expr) => {
                if let_extends(expr) || binds_by_reference(&local.pat) {
                    return Err(Reason::AcrossStatements);
                }
                if !local.attrs.is_empty() {
                    return Err(Reason::Unfollowed(
                        "a statement holding its guard has attributes",
                    ));
                }
                if let Some(group) = self.group_of(var) {
                    self.awaiting.push((group, local));
                }
                let range = self.text.of(expr).ok_or(UNLOCATED)?;
                let body = substitute(range.clone())?;
                (range, format!("{name}.{method}(|{name}| {body})"))
            }
            Root::Value(expr) | Root::Condition(expr) => {
                if self.attributed(expr) {
                    return Err(Reason::Unfollowed(
                        "a statement holding its guard has attributes",
                    ));
                }
                if matches!(plan.root, Root::Condition(_)) && contains_let(expr) {
                    return Err(Reason::Unfollowed("a guard is in a `let` chain"));
                }
                let range = self.text.of(expr).ok_or(UNLOCATED)?;
                let body = substitute(range.clone())?;
                (range, format!("{name}.{method}(|{name}| {body})"))
            }
        };
        Ok(self.edit(range, replacement))
    }

    /// Whether the node on top of the stack is inside a closure passed to a
    /// `with` or `read` method, which may be an actor's callback.
    fn inside_actor(&self) -> bool {
        self.stack
            .iter()
            .zip(self.stack.iter().skip(1))
            .any(|(node, below)| match (node, below) {
                (Node::Expr(Expr::MethodCall(call)), Node::Expr(arg)) => {
                    (call.method == "with" || call.method == "read")
                        && call.args.iter().any(|candidate| is(candidate, arg))
                }
                (Node::Expr(Expr::Call(call)), Node::Expr(arg)) => {
                    callee_name(&call.func).is_some_and(|name| name == "with" || name == "read")
                        && call.args.iter().any(|candidate| is(candidate, arg))
                }
                _ => false,
            })
    }

    /// Whether an expression starts with attributes, which can't move into a
    /// closure body.
    fn attributed(&self, expr: &Expr) -> bool {
        self.text
            .of(expr)
            .and_then(|range| self.text.get(range))
            .is_some_and(|text| text.starts_with('#'))
    }

    /// Whether the statement on the stack carries attributes.
    fn statement_attributes(&self) -> bool {
        self.stack.iter().rev().find_map(|node| match node {
            Node::Stmt(Stmt::Macro(mac)) => Some(!mac.attrs.is_empty()),
            Node::Stmt(Stmt::Local(local)) => Some(!local.attrs.is_empty()),
            Node::Stmt(_) => Some(false),
            _ => None,
        }) == Some(true)
    }

    fn block(&mut self, block: &'a Block, kind: BlockKind) {
        self.scopes.push(Scope {
            vars: Vec::new(),
            barrier: false,
        });
        self.items.push(ItemScope::new(
            block.stmts.iter().filter_map(|stmt| match stmt {
                Stmt::Item(item) => Some(item),
                _ => None,
            }),
            false,
        ));
        self.stack.push(Node::Block(block, kind));
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
        self.stack.pop();
        self.items.pop();
        self.scopes.pop();
    }

    /// A function body, with its parameters in scope and the enclosing
    /// function's locals out of it.
    fn function(&mut self, sig: &'a syn::Signature, body: &'a Block) {
        self.scopes.push(Scope {
            vars: Vec::new(),
            barrier: true,
        });
        for input in &sig.inputs {
            if let syn::FnArg::Typed(typed) = input {
                self.declare_pattern(&typed.pat);
            }
        }
        self.generic(&sig.generics, |walker| walker.block(body, BlockKind::Body));
        self.scopes.pop();
    }

    /// Runs `f` with `generics` in scope: a type parameter named `Arc`
    /// shadows an imported one.
    fn generic(&mut self, generics: &syn::Generics, f: impl FnOnce(&mut Self)) {
        let mut scope = ItemScope::new([], false);
        for param in &generics.params {
            let name = match param {
                syn::GenericParam::Type(param) => &param.ident,
                syn::GenericParam::Const(param) => &param.ident,
                syn::GenericParam::Lifetime(_) => continue,
            };
            scope.names.insert(name.to_string(), Binding::Item);
        }
        self.items.push(scope);
        f(self);
        self.items.pop();
    }

    /// The condition of an `if` or `while`, declaring what its `let`s bind.
    fn condition(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Binary(binary) if matches!(binary.op, BinOp::And(_)) => {
                self.stack.push(Node::Expr(expr));
                self.condition(&binary.left);
                self.condition(&binary.right);
                self.stack.pop();
            }
            Expr::Let(binding) => {
                self.stack.push(Node::Expr(expr));
                self.visit_expr(&binding.expr);
                self.stack.pop();
                self.declare_pattern(&binding.pat);
            }
            _ => self.visit_expr(expr),
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.scopes.push(Scope {
            vars: Vec::new(),
            barrier: false,
        });
        f(self);
        self.scopes.pop();
    }

    fn mac(&mut self, mac: &'a Macro) {
        let parsed = self
            .text
            .of(mac)
            .and_then(|range| self.macros.get(&range.start));
        let Some(parsed) = parsed else {
            self.mentions(mac.tokens.clone());
            return;
        };
        self.stack.push(Node::Macro(parsed));
        for (position, arg) in parsed.args.iter().enumerate() {
            if position == parsed.format
                && let Expr::Lit(lit) = arg
                && let Lit::Str(format) = &lit.lit
            {
                for name in format_captures(&format.value()) {
                    self.mention(&name, Reason::Unfollowed("it is named in a format string"));
                }
            } else if let Some(value) = parsed.named(position, arg) {
                self.visit_expr(value);
            } else {
                self.visit_expr(arg);
            }
        }
        self.stack.pop();
    }

    /// Every name in tokens that fix can't read as expressions counts as a
    /// use, including the variables a string might capture.
    fn mentions(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.mentions(group.stream()),
                TokenTree::Ident(ident) => {
                    self.mention(&bare(&ident), Reason::Unfollowed("it is used in a macro"));
                }
                TokenTree::Literal(literal) => {
                    if let Ok(Lit::Str(string)) =
                        syn::parse2::<Lit>(TokenTree::Literal(literal).into())
                    {
                        for name in format_captures(&string.value()) {
                            self.mention(&name, Reason::Unfollowed("it is used in a macro"));
                        }
                    }
                }
                TokenTree::Punct(_) => {}
            }
        }
    }
}

/// The last segment of a call's path, as `read` in `Actor::read(&a, f)`.
fn callee_name(func: &Expr) -> Option<String> {
    match func {
        Expr::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// An identifier's name without `r#`, which Rust resolves as the same name.
fn bare(ident: &proc_macro2::Ident) -> String {
    ident.unraw().to_string()
}

/// How many times `text` names `name`, as an identifier or as a variable
/// that a string captures, or `None` if it doesn't tokenize.
fn names_in(text: &str, name: &str) -> Option<usize> {
    fn count(tokens: TokenStream, name: &str) -> usize {
        tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Group(group) => count(group.stream(), name),
                TokenTree::Ident(ident) => usize::from(bare(&ident) == name),
                TokenTree::Literal(literal) => {
                    match syn::parse2::<Lit>(TokenTree::Literal(literal).into()) {
                        Ok(Lit::Str(string)) => format_captures(&string.value())
                            .iter()
                            .filter(|capture| *capture == name)
                            .count(),
                        _ => 0,
                    }
                }
                TokenTree::Punct(_) => 0,
            })
            .sum()
    }
    TokenStream::from_str(text)
        .ok()
        .map(|tokens| count(tokens, name))
}

fn is_unwrap(call: &syn::ExprMethodCall) -> bool {
    call.turbofish.is_none()
        && match call.method.to_string().as_str() {
            "unwrap" => call.args.is_empty(),
            "expect" => {
                call.args.len() == 1
                    && matches!(call.args.first(), Some(Expr::Lit(lit)) if matches!(lit.lit, Lit::Str(_)))
            }
            _ => false,
        }
}

fn contains_let(expr: &Expr) -> bool {
    match expr {
        Expr::Let(_) => true,
        Expr::Binary(binary) if matches!(binary.op, BinOp::And(_)) => {
            contains_let(&binary.left) || contains_let(&binary.right)
        }
        _ => false,
    }
}

/// Whether a `let` initializer extends the life of a temporary, such as
/// `let r = &*guard;`, which keeps the guard until `r` goes out of scope.
fn let_extends(expr: &Expr) -> bool {
    match expr {
        Expr::Reference(_) | Expr::RawAddr(_) => true,
        Expr::Paren(e) => let_extends(&e.expr),
        Expr::Cast(e) => let_extends(&e.expr),
        Expr::Tuple(e) => e.elems.iter().any(let_extends),
        Expr::Array(e) => e.elems.iter().any(let_extends),
        Expr::Struct(e) => e.fields.iter().any(|field| let_extends(&field.expr)),
        Expr::Call(e) => e.args.iter().any(let_extends),
        Expr::Block(e) => e
            .block
            .stmts
            .last()
            .is_some_and(|stmt| matches!(stmt, Stmt::Expr(expr, None) if let_extends(expr))),
        Expr::If(e) => {
            e.then_branch
                .stmts
                .last()
                .is_some_and(|stmt| matches!(stmt, Stmt::Expr(expr, None) if let_extends(expr)))
                || e.else_branch
                    .as_ref()
                    .is_some_and(|(_, other)| let_extends(other))
        }
        Expr::Match(e) => e.arms.iter().any(|arm| let_extends(&arm.body)),
        _ => false,
    }
}

fn binds_by_reference(pat: &Pat) -> bool {
    struct Reference(bool);
    impl<'ast> Visit<'ast> for Reference {
        fn visit_pat_ident(&mut self, ident: &'ast syn::PatIdent) {
            self.0 |= ident.by_ref.is_some();
            syn::visit::visit_pat_ident(self, ident);
        }
    }
    let mut found = Reference(false);
    found.visit_pat(pat);
    found.0
}

impl<'a> Visit<'a> for Walker<'a> {
    fn visit_item(&mut self, item: &'a Item) {
        // A `macro_rules!` body can name the locals in scope where it is
        // defined, so it is read without an item barrier.
        if let Item::Macro(mac) = item {
            self.mac(&mac.mac);
            return;
        }
        self.scopes.push(Scope {
            vars: Vec::new(),
            barrier: true,
        });
        match item {
            Item::Mod(module) => {
                if let Some((_, items)) = &module.content {
                    self.items.push(ItemScope::new(items, true));
                    for item in items {
                        self.visit_item(item);
                    }
                    self.items.pop();
                }
            }
            Item::Fn(function) => self.function(&function.sig, &function.block),
            Item::Impl(item) => self.generic(&item.generics, |walker| {
                syn::visit::visit_item_impl(walker, item);
            }),
            Item::Trait(item) => self.generic(&item.generics, |walker| {
                syn::visit::visit_item_trait(walker, item);
            }),
            _ => syn::visit::visit_item(self, item),
        }
        self.scopes.pop();
    }

    fn visit_impl_item_fn(&mut self, function: &'a syn::ImplItemFn) {
        self.function(&function.sig, &function.block);
    }

    fn visit_trait_item_fn(&mut self, function: &'a syn::TraitItemFn) {
        if let Some(body) = &function.default {
            self.function(&function.sig, body);
        }
    }

    fn visit_block(&mut self, block: &'a Block) {
        self.block(block, BlockKind::Plain);
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        self.stack.push(Node::Stmt(stmt));
        match stmt {
            Stmt::Local(local) => self.local_statement(local),
            Stmt::Item(item) => self.visit_item(item),
            Stmt::Expr(expr, _) => self.visit_expr(expr),
            Stmt::Macro(mac) => self.mac(&mac.mac),
        }
        self.stack.pop();
    }

    fn visit_macro(&mut self, mac: &'a Macro) {
        self.mac(mac);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Some(var) = self.guard_var(expr) {
            self.stack.push(Node::Expr(expr));
            self.guard(var, expr);
            self.stack.pop();
            return;
        }
        if let Some((var, replacement)) = self.unwrapped(expr) {
            if let Some(entry) = self.vars.get_mut(var) {
                entry.used = true;
            }
            match self.text.of(expr) {
                Some(range) => {
                    let edit = self.edit(range, replacement);
                    self.add_edit(var, edit);
                }
                None => self.refuse(var, UNLOCATED),
            }
            return;
        }
        if let Some(var) = self.dropped(expr) {
            if let Some(entry) = self.vars.get_mut(var) {
                entry.used = true;
            }
            return;
        }
        self.stack.push(Node::Expr(expr));
        match expr {
            Expr::Path(path) => {
                if path.qself.is_none()
                    && let Some(name) = path.path.get_ident()
                {
                    self.mention(
                        &bare(name),
                        Reason::Unfollowed(
                            "it is passed, returned or stored where fix can't follow it",
                        ),
                    );
                }
            }
            Expr::Closure(closure) => self.scoped(|walker| {
                for input in &closure.inputs {
                    walker.declare_pattern(input);
                }
                match &*closure.body {
                    Expr::Block(body) if body.attrs.is_empty() && body.label.is_none() => {
                        walker.stack.push(Node::Expr(&closure.body));
                        walker.block(&body.block, BlockKind::Body);
                        walker.stack.pop();
                    }
                    body => walker.visit_expr(body),
                }
            }),
            Expr::Async(block) => self.block(&block.block, BlockKind::Body),
            Expr::If(branch) => {
                self.scoped(|walker| {
                    walker.condition(&branch.cond);
                    walker.block(&branch.then_branch, BlockKind::Body);
                });
                if let Some((_, other)) = &branch.else_branch {
                    match &**other {
                        Expr::Block(block) if block.attrs.is_empty() && block.label.is_none() => {
                            self.stack.push(Node::Expr(other));
                            self.block(&block.block, BlockKind::Body);
                            self.stack.pop();
                        }
                        other => self.visit_expr(other),
                    }
                }
            }
            Expr::While(lp) => self.scoped(|walker| {
                walker.condition(&lp.cond);
                walker.block(&lp.body, BlockKind::Body);
            }),
            Expr::ForLoop(lp) => {
                self.visit_expr(&lp.expr);
                self.scoped(|walker| {
                    walker.declare_pattern(&lp.pat);
                    walker.block(&lp.body, BlockKind::Body);
                });
            }
            Expr::Loop(lp) => self.block(&lp.body, BlockKind::Body),
            Expr::Match(m) => {
                self.visit_expr(&m.expr);
                for arm in &m.arms {
                    self.scoped(|walker| {
                        walker.declare_pattern(&arm.pat);
                        if let Pat::Guard(guard) = &arm.pat {
                            walker.visit_expr(&guard.guard);
                        }
                        walker.visit_expr(&arm.body);
                    });
                }
            }
            Expr::Macro(mac) => self.mac(&mac.mac),
            _ => syn::visit::visit_expr(self, expr),
        }
        self.stack.pop();
    }
}

/// What the code that would run under a guard does, apart from the guard.
struct Scan<'w, 'a> {
    walker: &'w Walker<'a>,
    guard: &'a Expr,
    /// Inside a closure or async block, which doesn't run here.
    deferred: usize,
    /// Inside code that runs only on some paths: a branch, a match arm, the
    /// second operand of `&&` or `||`, or a loop body.
    branch: usize,
    awaits: bool,
    locks: bool,
    leaves: bool,
    unfollowed: Option<&'static str>,
}

impl<'w, 'a> Scan<'w, 'a> {
    fn new(walker: &'w Walker<'a>, guard: &'a Expr) -> Self {
        Self {
            walker,
            guard,
            deferred: 0,
            branch: 0,
            awaits: false,
            locks: false,
            leaves: false,
            unfollowed: None,
        }
    }

    fn full(&mut self, full: Full<'a>) {
        match full {
            Full::Expr(expr) => self.visit_expr(expr),
            Full::Macro(mac) => self.visit_macro(mac),
        }
    }
}

impl<'a> Visit<'a> for Scan<'_, 'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if is(expr, self.guard) {
            return;
        }
        match expr {
            Expr::Await(_) if self.deferred == 0 => self.awaits = true,
            Expr::Try(_)
            | Expr::Return(_)
            | Expr::Break(_)
            | Expr::Continue(_)
            | Expr::Yield(_)
                if self.deferred == 0 =>
            {
                self.leaves = true;
            }
            Expr::MethodCall(call) if LOCKS.contains(&call.method.to_string().as_str()) => {
                self.locks = true;
            }
            // `Actor::read(&a, f)` and `Mutex::lock(&m)` take locks too.
            Expr::Call(call)
                if callee_name(&call.func).is_some_and(|name| LOCKS.contains(&name.as_str())) =>
            {
                self.locks = true;
            }
            Expr::Call(call) if self.walker.local(&call.func).is_some() => {
                // A local closure might use this or another actor.
                self.unfollowed
                    .get_or_insert("a statement holding its guard calls a local closure");
            }
            Expr::Closure(_) | Expr::Async(_) => {
                self.deferred += 1;
                syn::visit::visit_expr(self, expr);
                self.deferred -= 1;
                return;
            }
            // A closure captures a variable it may move, so a value moved on
            // only one path would be dropped when the closure returns rather
            // than at the end of its scope.
            Expr::Path(_)
                if self.branch > 0 && self.deferred == 0 && self.walker.local(expr).is_some() =>
            {
                self.unfollowed.get_or_insert(
                    "a statement holding its guard may move a variable on only one path",
                );
            }
            Expr::If(branch) => {
                self.visit_expr(&branch.cond);
                self.branch += 1;
                self.visit_block(&branch.then_branch);
                if let Some((_, other)) = &branch.else_branch {
                    self.visit_expr(other);
                }
                self.branch -= 1;
                return;
            }
            Expr::Match(m) => {
                self.visit_expr(&m.expr);
                self.branch += 1;
                for arm in &m.arms {
                    self.visit_arm(arm);
                }
                self.branch -= 1;
                return;
            }
            Expr::Binary(binary) if matches!(binary.op, BinOp::And(_) | BinOp::Or(_)) => {
                self.visit_expr(&binary.left);
                self.branch += 1;
                self.visit_expr(&binary.right);
                self.branch -= 1;
                return;
            }
            Expr::While(_) | Expr::ForLoop(_) | Expr::Loop(_) => {
                self.branch += 1;
                syn::visit::visit_expr(self, expr);
                self.branch -= 1;
                return;
            }
            _ => {}
        }
        syn::visit::visit_expr(self, expr);
    }

    fn visit_macro(&mut self, mac: &'a Macro) {
        let parsed = self
            .walker
            .text
            .of(mac)
            .and_then(|range| self.walker.macros.get(&range.start));
        match parsed {
            Some(parsed) => {
                for (position, arg) in parsed.args.iter().enumerate() {
                    let conditional = usize::from(position >= parsed.conditional);
                    self.branch += conditional;
                    match parsed.named(position, arg) {
                        Some(value) => self.visit_expr(value),
                        None => self.visit_expr(arg),
                    }
                    self.branch -= conditional;
                }
            }
            None => {
                self.unfollowed
                    .get_or_insert("a statement holding its guard calls a macro fix can't read");
            }
        }
    }

    fn visit_item(&mut self, _: &'a Item) {}
}

/// Removes `std::sync::Mutex` and `std::sync::Arc` imports that the rewrite
/// leaves unused.
struct Imports<'t> {
    text: &'t Text<'t>,
    /// Byte offsets of every identifier in the file, by name.
    idents: HashMap<String, Vec<Range<usize>>>,
}

impl<'t> Imports<'t> {
    fn prune(text: &'t Text<'t>, file: &Path, tree: &syn::File, edits: &[Edit]) -> Vec<Edit> {
        let mut uses = Uses(Vec::new());
        uses.visit_file(tree);
        let mut imports = Imports {
            text,
            idents: HashMap::new(),
        };
        if let Some(body) = text.source.get(text.prefix..)
            && let Ok(tokens) = TokenStream::from_str(body)
        {
            let paths: Vec<Range<usize>> =
                uses.0.iter().filter_map(|item| text.of(*item)).collect();
            imports.collect(tokens, &paths);
        }
        // A `use` path's first name can refer to another import, as `Arc`
        // does in `use Arc as Shared;`.
        for item in &uses.0 {
            if item.leading_colon.is_none() {
                imports.roots(&item.tree);
            }
        }
        let mut pruned = Vec::new();
        let mut lines = Vec::new();
        for item in uses.0 {
            if !matches!(item.vis, syn::Visibility::Inherited) {
                continue;
            }
            let mut removed = BTreeSet::new();
            imports.unused(&item.tree, &mut Vec::new(), edits, &mut removed);
            if removed.is_empty() {
                continue;
            }
            let edit = match imports.rewrite(&item.tree, &removed) {
                Some(Some(tree)) => text.of(&item.tree).map(|range| (range, tree)),
                Some(None) => None,
                None => imports.line(item).map(|range| (range, String::new())),
            };
            match edit {
                Some((range, replacement)) if replacement.is_empty() => lines.push(range),
                Some((range, replacement)) => pruned.push((range, replacement)),
                None => {}
            }
        }
        // Lines removed together leave at most the blank line that separated
        // them from what follows, and none at the top of the file.
        lines.sort_by_key(|range| range.start);
        let mut runs: Vec<Range<usize>> = Vec::new();
        for range in lines {
            match runs.last_mut() {
                Some(run) if run.end == range.start => run.end = range.end,
                _ => runs.push(range),
            }
        }
        for run in runs {
            let run = imports.without_blank_line(run);
            pruned.push((run, String::new()));
        }
        pruned
            .into_iter()
            .map(|(range, replacement)| Edit {
                file: file.to_path_buf(),
                start: range.start,
                end: range.end,
                replacement,
            })
            .collect()
    }

    /// Records the identifiers that could name an import: not those in `use`
    /// items, nor those after `::` in a longer path, except a path through
    /// `self`, `super` or `crate`, which can reach the import.
    fn collect(&mut self, tokens: TokenStream, uses: &[Range<usize>]) {
        let mut colons = 0;
        let mut module = false;
        for token in tokens {
            match token {
                TokenTree::Group(group) => {
                    module = false;
                    self.collect(group.stream(), uses);
                }
                TokenTree::Ident(ident) => {
                    let reachable = colons < 2 || module;
                    module = ident == "self" || ident == "super" || ident == "crate";
                    if reachable
                        && let Some(range) = self.text.range(ident.span())
                        && !uses
                            .iter()
                            .any(|item| item.start <= range.start && range.end <= item.end)
                    {
                        self.idents
                            .entry(ident.to_string())
                            .or_default()
                            .push(range);
                    }
                }
                TokenTree::Punct(ref punct) if punct.as_char() == ':' => {
                    colons += 1;
                    continue;
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => module = false,
            }
            colons = 0;
        }
    }

    fn roots(&mut self, tree: &UseTree) {
        let ident = match tree {
            UseTree::Path(path) => &path.ident,
            UseTree::Name(name) => &name.ident,
            UseTree::Rename(rename) => &rename.ident,
            UseTree::Glob(_) => return,
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.roots(tree);
                }
                return;
            }
        };
        if let Some(range) = self.text.range(ident.span()) {
            self.idents
                .entry(ident.to_string())
                .or_default()
                .push(range);
        }
    }

    /// Collects the starts of leaves importing `Arc` or `Mutex` whose name
    /// appears nowhere else in the file once `edits` apply.
    fn unused(
        &self,
        tree: &UseTree,
        prefix: &mut Vec<String>,
        edits: &[Edit],
        removed: &mut BTreeSet<usize>,
    ) {
        let (ident, name) = match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.unused(&path.tree, prefix, edits, removed);
                prefix.pop();
                return;
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.unused(tree, prefix, edits, removed);
                }
                return;
            }
            UseTree::Name(name) => (&name.ident, &name.ident),
            UseTree::Rename(rename) => (&rename.ident, &rename.rename),
            UseTree::Glob(_) => return,
        };
        let mut path = prefix.clone();
        let name = if ident == "self" {
            match (tree, prefix.last()) {
                (UseTree::Name(_), Some(module)) => module.clone(),
                _ => name.to_string(),
            }
        } else {
            path.push(ident.to_string());
            name.to_string()
        };
        // The rewrite can leave unused only the imports of the module and the
        // two types it replaces.
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        if !matches!(
            path.as_slice(),
            ["std" | "alloc", "sync"] | ["std" | "alloc", "sync", "Arc"] | ["std", "sync", "Mutex"]
        ) {
            return;
        }
        let Some(leaf) = self.text.of(tree) else {
            return;
        };
        // Text an edit replaces is gone, but its replacement can keep a name,
        // as `Actor::new(Arc::new(1))` keeps `Arc`.
        let elsewhere = self.idents.get(&name).into_iter().flatten().any(|range| {
            !(leaf.start <= range.start && range.end <= leaf.end)
                && !edits
                    .iter()
                    .any(|edit| edit.start <= range.start && range.end <= edit.end)
        }) || edits
            .iter()
            .any(|edit| names_in(&edit.replacement, &name) != Some(0));
        if !elsewhere {
            removed.insert(leaf.start);
        }
    }

    /// The tree without the removed leaves: `None` when nothing is left,
    /// `Some(None)` when unchanged.
    fn rewrite(&self, tree: &UseTree, removed: &BTreeSet<usize>) -> Option<Option<String>> {
        match tree {
            UseTree::Path(path) => match self.rewrite(&path.tree, removed)? {
                Some(rest) => Some(Some(format!("{}::{rest}", path.ident))),
                None => Some(None),
            },
            UseTree::Name(_) | UseTree::Rename(_) => {
                let start = self.text.of(tree)?.start;
                (!removed.contains(&start)).then_some(None)
            }
            UseTree::Glob(_) => Some(None),
            UseTree::Group(group) => {
                let mut kept = Vec::new();
                let mut changed = false;
                for tree in &group.items {
                    match self.rewrite(tree, removed) {
                        None => changed = true,
                        Some(None) => kept.push(self.text.get(self.text.of(tree)?)?.to_owned()),
                        Some(Some(text)) => {
                            changed = true;
                            kept.push(text);
                        }
                    }
                }
                match kept.as_slice() {
                    _ if !changed => Some(None),
                    [] => None,
                    [only] if !only.starts_with("self") => Some(Some(only.clone())),
                    kept => Some(Some(format!("{{{}}}", kept.join(", ")))),
                }
            }
        }
    }

    /// An item's text with its indentation and line break, when the item
    /// has its own line.
    fn line(&self, item: &ItemUse) -> Option<Range<usize>> {
        let range = self.text.of(item)?;
        let source = self.text.source;
        let line_start = source.get(..range.start)?.rfind('\n').map_or(0, |n| n + 1);
        let start = if source.get(line_start..range.start)?.trim().is_empty() {
            line_start
        } else {
            range.start
        };
        if start != line_start {
            return Some(range);
        }
        Some(start..range.end + line_break(source.get(range.end..)?))
    }

    /// Extends whole lines to remove over the blank line below them when
    /// they follow a blank line or start the file.
    fn without_blank_line(&self, lines: Range<usize>) -> Range<usize> {
        let source = self.text.source;
        let whole = lines.start == 0
            || source
                .get(..lines.start)
                .is_some_and(|above| above.ends_with('\n'));
        let above = source.get(..lines.start).unwrap_or_default();
        let blank_above =
            above.len() <= self.text.prefix || above.ends_with("\n\n") || above.ends_with("\n\r\n");
        let blank_below = source.get(lines.end..).map_or(0, line_break);
        if whole && blank_above && blank_below > 0 {
            lines.start..lines.end + blank_below
        } else {
            lines
        }
    }
}

/// The length of the line break that `text` starts with.
fn line_break(text: &str) -> usize {
    if text.starts_with("\r\n") {
        2
    } else {
        usize::from(text.starts_with('\n'))
    }
}

struct Uses<'ast>(Vec<&'ast ItemUse>);

impl<'ast> Visit<'ast> for Uses<'ast> {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        self.0.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plans `source` as Clippy would see it, with every occurrence of each
    /// of `mutexes` resolved to `std::sync::Mutex`, and applies the edits.
    fn fix(source: &str, mutexes: &[&str]) -> (String, Vec<(String, Reason)>) {
        let mut spans = BTreeSet::new();
        for mutex in mutexes {
            for (start, _) in source.match_indices(mutex) {
                spans.insert((start, start + mutex.len()));
            }
        }
        let outcome = plan(Path::new("lib.rs"), source, &spans);
        let mut edits = outcome.edits;
        edits.sort();
        let mut fixed = source.to_owned();
        for edit in edits.iter().rev() {
            fixed.replace_range(edit.start..edit.end, &edit.replacement);
        }
        let refusals = outcome
            .refusals
            .into_iter()
            .map(|refusal| (refusal.name, refusal.reason))
            .collect();
        (fixed, refusals)
    }

    /// Asserts that nothing in `body` changes and that `name` is refused
    /// for `reason`.
    fn refused(body: &str, name: &str, reason: Reason) {
        let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\n");
        let (fixed, refusals) = fix(&source, &["Mutex"]);
        assert_eq!(fixed, source);
        assert!(
            refusals.contains(&(name.to_owned(), reason)),
            "{refusals:?}\n{source}"
        );
    }

    #[test]
    fn rewrites_the_guide_example_and_drops_the_imports() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\n\nfn f() {\n    let hits = Arc::new(Mutex::new(0u64));\n    let handle = Arc::clone(&hits);\n    *handle.lock().unwrap() += 1;\n    assert_eq!(*hits.lock().unwrap(), 1);\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "fn f() {\n    let hits = ::adamantium::Actor::new(0u64);\n    let handle = hits.clone();\n    handle.with(|handle| *handle += 1);\n    hits.read(|hits| { assert_eq!(*hits, 1); });\n}\n"
        );
    }

    #[test]
    fn shadowed_names_are_other_variables() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f(items: &[u8]) {\n    let m = Arc::new(Mutex::new(0usize));\n    { let m = 5; consume(m); }\n    items.iter().for_each(|m| consume(*m));\n    match items.len() { m => consume(m) }\n    if let Some(m) = items.first() { consume(m); }\n    *m.lock().unwrap() += 1;\n}\nfn consume<T>(_: T) {}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("m.with(|m| *m += 1);"), "{fixed}");
        assert!(fixed.contains("{ let m = 5; consume(m); }"), "{fixed}");
    }

    #[test]
    fn imports_and_aliases_resolve_through_the_file() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc as Shared, Mutex as Lock};\nmod inner {\n    use super::*;\n    fn f() -> u8 {\n        let state: Shared<Lock<u8>> = Shared::new(Lock::new(1));\n        let copy = Shared::clone(&state);\n        let value = *copy.lock().unwrap();\n        value\n    }\n}\n",
            &["Lock", "Lock<u8>"],
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "mod inner {\n    use super::*;\n    fn f() -> u8 {\n        let state: ::adamantium::Actor<u8> = ::adamantium::Actor::new(1);\n        let copy = state.clone();\n        let value = copy.read(|copy| *copy);\n        value\n    }\n}\n"
        );
    }

    #[test]
    fn an_arc_fix_cannot_resolve_is_left_alone() {
        for source in [
            "use std::sync::Mutex;\nstruct Arc<T>(T);\nimpl<T> Arc<T> { fn new(t: T) -> Self { Self(t) } }\nfn f() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
            "use std::sync::Mutex;\nuse other::*;\nfn f() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
            "use std::sync::Mutex;\nuse triomphe::Arc;\nfn f() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
        ] {
            let (fixed, refusals) = fix(source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert_eq!(refusals, []);
        }
    }

    #[test]
    fn guards_beyond_one_statement_are_refused() {
        for body in [
            "fn f() { let m = Arc::new(Mutex::new(0)); let mut g = m.lock().unwrap(); *g += 1; }",
            "fn f() { let m = Arc::new(Mutex::new(0)); match *m.lock().unwrap() { 0 => {} _ => {} } }",
            "fn f() { let m = Arc::new(Mutex::new(vec![1])); if let Some(x) = m.lock().unwrap().pop() { drop(x); } }",
            "fn f() { let m = Arc::new(Mutex::new(vec![1])); for x in m.lock().unwrap().iter() { drop(x); } }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let r = &*m.lock().unwrap(); drop(r); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let ref r = *m.lock().unwrap(); drop(r); }",
        ] {
            refused(body, "m", Reason::AcrossStatements);
        }
    }

    #[test]
    fn awaits_second_locks_and_control_flow_are_refused() {
        refused(
            "async fn f() { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push(g().await); }\nasync fn g() -> u8 { 1 }",
            "m",
            Reason::AcrossAwait,
        );
        let both = "fn f() { let a = Arc::new(Mutex::new(1)); let b = Arc::new(Mutex::new(2)); *a.lock().unwrap() += *b.lock().unwrap(); }";
        refused(both, "a", Reason::TwoLocks);
        refused(both, "b", Reason::TwoLocks);
        refused(
            "struct S { x: Mutex<u8> }\nfn f(s: &S) { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += *s.x.lock().unwrap(); }",
            "m",
            Reason::TwoLocks,
        );
        refused(
            "fn f() -> Option<()> { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push(g()?); Some(()) }\nfn g() -> Option<u8> { None }",
            "m",
            Reason::ControlFlow,
        );
    }

    #[test]
    fn uses_fix_cannot_follow_refuse_every_clone() {
        for body in [
            "fn f() { let m = Arc::new(Mutex::new(0)); consume(m.clone()); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let n = Arc::strong_count(&m); consume(n); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); consume(Holder { m }); }\nstruct Holder { m: Arc<Mutex<i32>> }",
            "fn f() -> Arc<Mutex<i32>> { let m = Arc::new(Mutex::new(0)); m }",
            "fn f() { let m = Arc::new(Mutex::new(0)); println!(\"{m:?}\"); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); other!(m); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let h = m.clone(); *m.lock().unwrap() += 1; consume(h); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let g = || 1; *m.lock().unwrap() += g(); }",
            "fn f() { let m = Arc::new(Mutex::new(vec![1])); let n = m.lock().unwrap().len(); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); if let Ok(mut g) = m.lock() { *g += 1; } }",
            "fn f() { #[cfg(test)] let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }",
        ] {
            let source =
                format!("use std::sync::{{Arc, Mutex}};\n{body}\nfn consume<T>(_: T) {{}}\n");
            let (fixed, refusals) = fix(&source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert!(
                refusals
                    .iter()
                    .any(|(name, reason)| name == "m" && matches!(reason, Reason::Unfollowed(_))),
                "{refusals:?}\n{source}"
            );
        }
    }

    #[test]
    fn adjacent_imports_are_removed_in_one_edit() {
        let source = "use std::sync::Mutex;\nuse std::sync::Arc;\n\nfn f() {\n    let m = Arc::new(Mutex::new(1));\n    drop(m);\n}\n";
        let outcome = plan(
            Path::new("lib.rs"),
            source,
            &source
                .match_indices("Mutex")
                .map(|(start, _)| (start, start + "Mutex".len()))
                .collect(),
        );
        assert_eq!(outcome.edits.len(), 2);
        let (fixed, _) = fix(source, &["Mutex"]);
        assert_eq!(
            fixed,
            "fn f() {\n    let m = ::adamantium::Actor::new(1);\n    drop(m);\n}\n"
        );
    }

    #[test]
    fn a_statement_naming_the_binding_again_is_refused() {
        for body in [
            "fn f() { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push({ let h = m.clone(); drop(h); 1 }); }",
            "fn f() { let m = Arc::new(Mutex::new(vec![1])); m.lock().unwrap().push(format!(\"{m:?}\").len()); }",
        ] {
            let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\n");
            let (fixed, refusals) = fix(&source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert!(
                refusals.iter().any(|(name, reason)| name == "m"
                    && *reason
                        == Reason::Unfollowed("a statement holding its guard names it again")),
                "{refusals:?}"
            );
        }
    }

    #[test]
    fn guards_inside_an_actor_callback_are_refused() {
        refused(
            "fn f(a: &adamantium::Actor<u8>) { a.with(|_| { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }); }",
            "m",
            Reason::InsideActor,
        );
    }

    #[test]
    fn call_arguments_are_reborrowed_not_copied() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn bump(x: &mut i32) { *x += 1; }\nfn f(mut x: i32) {\n    let m = Arc::new(Mutex::new(&mut x));\n    bump(*m.lock().unwrap());\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("m.with(|m| { bump(*m); });"), "{fixed}");
    }

    #[test]
    fn unsized_state_is_left_alone() {
        for body in [
            "fn f() { let m: Arc<Mutex<[u8]>> = Arc::new(Mutex::new([1u8, 2])); assert_eq!(m.lock().unwrap().len(), 2); }",
            "fn f() { let m: Arc<Mutex<dyn std::fmt::Debug>> = Arc::new(Mutex::new(1)); drop(m); }",
            "type Bytes = [u8];\nfn f() { let m: Arc<Mutex<Bytes>> = Arc::new(Mutex::new([1u8])); drop(m); }",
        ] {
            let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\n");
            let (fixed, _) = fix(
                &source,
                &[
                    "Mutex",
                    "Mutex<[u8]>",
                    "Mutex<dyn std::fmt::Debug>",
                    "Mutex<Bytes>",
                ],
            );
            assert_eq!(fixed, source);
        }
    }

    #[test]
    fn imports_that_other_imports_name_are_kept() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nuse Arc as Shared;\nfn f() -> Shared<u8> {\n    let m = Arc::new(Mutex::new(1));\n    drop(m);\n    Shared::new(1)\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.starts_with("use std::sync::Arc;\nuse Arc as Shared;\n"),
            "{fixed}"
        );
    }

    #[test]
    fn a_write_destination_is_borrowed_mutably() {
        let (fixed, refusals) = fix(
            "use std::fmt::Write;\nuse std::sync::{Arc, Mutex};\nfn f() -> std::fmt::Result {\n    let m = Arc::new(Mutex::new(String::new()));\n    write!(*m.lock().unwrap(), \"hello\").unwrap();\n    writeln!(String::new(), \"{}\", *m.lock().unwrap())\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("m.with(|m| { write!(*m, \"hello\").unwrap(); });"),
            "{fixed}"
        );
        assert!(
            fixed.contains("m.read(|m| writeln!(String::new(), \"{}\", *m))"),
            "{fixed}"
        );
    }

    #[test]
    fn imports_named_through_module_paths_are_kept() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() -> self::Arc<u8> {\n    let m = Arc::new(Mutex::new(1));\n    drop(m);\n    self::Arc::new(1)\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.starts_with("use std::sync::Arc;\nfn f()"), "{fixed}");
    }

    #[test]
    fn a_guard_clone_keeps_its_method_lookup() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f(mut value: Vec<i32>) -> Vec<i32> {\n    let m = Arc::new(Mutex::new(&mut value));\n    let copy: Vec<i32> = m.lock().unwrap().clone();\n    copy\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("let copy: Vec<i32> = m.with(|m| (*m).clone());"),
            "{fixed}"
        );
    }

    #[test]
    fn assignments_in_macro_arguments_are_scanned() {
        refused(
            "fn f(a: &adamantium::Actor<u8>, mut v: [u32; 1]) { let m = Arc::new(Mutex::new(1u32)); assert_eq!(v[a.read(|_| 0)] = *m.lock().unwrap(), ()); }",
            "m",
            Reason::TwoLocks,
        );
    }

    #[test]
    fn imports_kept_by_replacement_text_stay() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() {\n    let m = Arc::new(Mutex::new(Arc::new(1)));\n    drop(m);\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "use std::sync::Arc;\nfn f() {\n    let m = ::adamantium::Actor::new(Arc::new(1));\n    drop(m);\n}\n"
        );
    }

    #[test]
    fn values_moved_on_one_path_are_refused() {
        refused(
            "struct D;\nfn f(flag: bool, d: D) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().extend(if flag { Some(d) } else { None }); }",
            "m",
            Reason::Unfollowed(
                "a statement holding its guard may move a variable on only one path",
            ),
        );
    }

    #[test]
    fn a_parenthesized_guard_is_still_the_guard() {
        refused(
            "fn f() { let m = Arc::new(Mutex::new(1)); let g = (m.lock().unwrap()); println!(\"{}\", *g); }",
            "m",
            Reason::AcrossStatements,
        );
    }

    #[test]
    fn a_clone_that_takes_mut_self_keeps_mutable_access() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct State(u32);\nimpl State { fn clone(&mut self) -> u32 { self.0 += 1; self.0 } }\nfn f() -> u32 {\n    let m = Arc::new(Mutex::new(State(0)));\n    let n = m.lock().unwrap().clone();\n    n\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("let n = m.with(|m| (*m).clone());"),
            "{fixed}"
        );
    }

    #[test]
    fn a_local_macro_naming_the_binding_is_refused() {
        refused(
            "fn f() { let m = Arc::new(Mutex::new(0)); macro_rules! bump { () => { *m.lock().unwrap() += 1 }; } bump!(); }",
            "m",
            Reason::Unfollowed("it is used in a macro"),
        );
    }

    #[test]
    fn a_type_parameter_shadows_an_imported_arc() {
        for source in [
            "use std::sync::{Arc, Mutex};\ntrait Make { fn new(value: Mutex<i32>) -> Self; }\nfn f<Arc: Make>() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
            "use std::sync::{Arc, Mutex};\ntrait Make { fn new(value: Mutex<i32>) -> Self; }\nstruct S<T>(T);\nimpl<Arc: Make> S<Arc> { fn f() { let m = Arc::new(Mutex::new(1)); drop(m); } }\n",
        ] {
            let (fixed, refusals) = fix(source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert_eq!(refusals, []);
        }
    }

    #[test]
    fn conditional_or_conflicting_names_are_ambiguous() {
        for source in [
            "use std::sync::Mutex;\nstruct Arc<T>(T);\nimpl<T> Arc<T> { fn new(t: T) -> Self { Self(t) } }\n#[cfg(any())]\nuse std::sync::Arc;\nfn f() { let m = Arc::new(Mutex::new(1u8)); drop(m); }\n",
            "use std::sync::Mutex;\n#[cfg(any())]\nuse std::sync::Arc;\nfn f() { let m = Arc::new(Mutex::new(1u8)); drop(m); }\n",
        ] {
            let (fixed, refusals) = fix(source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert_eq!(refusals, []);
        }
    }

    #[test]
    fn a_clone_that_borrows_stays_inside_its_statement() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct S;\nimpl S { fn clone(&self) -> &Self { self } }\nfn consume(_: &S) {}\nfn f() {\n    let m = Arc::new(Mutex::new(S));\n    consume(m.lock().unwrap().clone());\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("m.with(|m| { consume((*m).clone()); });"),
            "{fixed}"
        );
    }

    #[test]
    fn associated_function_lock_calls_count() {
        refused(
            "fn f(a: &adamantium::Actor<u8>) { let m = Arc::new(Mutex::new(0u8)); *m.lock().unwrap() += adamantium::Actor::read(a, |v| *v); }",
            "m",
            Reason::TwoLocks,
        );
        refused(
            "fn f(a: &adamantium::Actor<u8>) { adamantium::Actor::with(a, |_| { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }); }",
            "m",
            Reason::InsideActor,
        );
    }

    #[test]
    fn assertion_messages_and_debug_assertions_are_conditional() {
        for body in [
            "struct D;\nfn f(d: D) { let m = Arc::new(Mutex::new(true)); assert!(*m.lock().unwrap(), \"{}\", { drop(d); \"bad\" }); }",
            "struct D;\nfn f(d: D) { let m = Arc::new(Mutex::new(1)); debug_assert_eq!(*m.lock().unwrap(), { drop(d); 1 }); }",
        ] {
            refused(
                body,
                "m",
                Reason::Unfollowed(
                    "a statement holding its guard may move a variable on only one path",
                ),
            );
        }
    }

    #[test]
    fn raw_identifiers_name_the_same_variable() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() {\n    let r#m = Arc::new(Mutex::new(0));\n    *m.lock().unwrap() += 1;\n    let h = r#m.clone();\n    drop(h);\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "fn f() {\n    let r#m = ::adamantium::Actor::new(0);\n    r#m.with(|r#m| *r#m += 1);\n    let h = r#m.clone();\n    drop(h);\n}\n"
        );
    }

    #[test]
    fn format_captures_name_variables() {
        assert_eq!(
            format_captures("{a} {{b}} {c:?} {:>width$} {d:.prec$} {0}"),
            ["a", "c", "width", "d", "prec"]
        );
    }
}
