//! Shared-state rewrites: local `Arc<Mutex<T>>` bindings and private struct
//! fields become `Actor<T>`.
//!
//! A binding is rewritten only when every use of it, and of every clone of
//! it, has one of the forms the migration guide marks as mechanical: the
//! constructor, clones, a guard used within one statement, a copy of the state, and
//! taking the state back. Any other use leaves the binding and all of its
//! clones unchanged, and the caller reports why with a link to the guide.
//! Fields follow the same rules, with the extra checks in `fields`, and a
//! field and the locals holding its state stand or fall together. So do a
//! private function's parameters and return value and the values its calls
//! pass and take, with the checks in `calls`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr,
};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::{
    BinOp, Block, Expr, ExprLit, GenericArgument, Item, ItemUse, Lit, Local, Macro, Pat,
    PathArguments, Stmt, Type, UnOp, UseTree, ext::IdentExt, punctuated::Punctuated,
    spanned::Spanned, visit::Visit,
};

use quote::ToTokens;

use super::{
    Edit,
    scan::{Crate, Hazard, macro_attribute},
};

mod calls;
mod fields;

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
    /// The statement that takes the guard calls a function, or a method on
    /// a value other than the state, which might call an actor and would
    /// then panic inside the callback.
    Calls,
    /// Code that locks a field might run inside an actor's callback, where
    /// the field's own actor would then panic.
    Nested(&'static str),
    /// The enclosing future has no proved polling context.
    Poll,
    /// Any other use that the rewrite can't follow.
    Unfollowed(&'static str),
}

impl Reason {
    /// The migration guide's section for this case.
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::AcrossStatements => "a-guard-held-across-statements",
            Self::AcrossAwait => "a-lock-held-across-an-await",
            Self::TwoLocks | Self::InsideActor | Self::Calls | Self::Nested(_) => {
                "two-locks-at-once"
            }
            Self::ControlFlow | Self::Unfollowed(_) | Self::Poll => "shared-state",
        }
    }

    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::AcrossStatements => "a guard of it lives beyond one statement",
            Self::AcrossAwait => "a statement holding its guard also awaits",
            Self::TwoLocks => "a statement holding its guard takes another lock",
            Self::ControlFlow => "a statement holding its guard can return, break or use `?`",
            Self::InsideActor => "it is locked inside another actor's `with` or `read` callback",
            Self::Calls => {
                "a statement holding its guard calls a function or another value's method, which might use an actor"
            }
            Self::Nested(detail) | Self::Unfollowed(detail) => detail,
            Self::Poll => "its future polling context is unproved; the rewrite remains manual",
        }
    }
}

/// A binding left as it was.
pub(super) struct Refusal {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) reason: Reason,
    pub(super) boundary: Option<String>,
}

/// The edits for one file, applied together, and the bindings left alone.
pub(super) struct Outcome {
    pub(super) edits: Vec<Edit>,
    pub(super) refusals: Vec<Refusal>,
}

/// One file's rewrites before the checks that need the whole package.
pub(super) struct Draft {
    file: PathBuf,
    source: String,
    groups: Vec<Group>,
    /// Groups that hold one state, such as a field and a local it is
    /// cloned into, which stand or fall together.
    links: Vec<(usize, usize)>,
    /// The functions with parameters or a return value to rewrite, each
    /// with how many of its calls fix followed and its groups.
    callees: Vec<(String, usize, Vec<usize>)>,
}

const ACTOR: &str = "::adamantium::Actor";

/// A node whose text fix couldn't find, which only a parser bug would cause.
const UNLOCATED: Reason = Reason::Unfollowed("fix couldn't find its source text");

/// Why a group that holds the same state as a refused one is refused.
const SHARED: Reason =
    Reason::Unfollowed("it holds the same state as a value fix leaves for review");

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
pub(super) fn plan(
    file: &Path,
    source: &str,
    mutexes: &BTreeSet<(usize, usize)>,
    krate: &Crate,
) -> Draft {
    let mut draft = Draft {
        file: file.to_path_buf(),
        source: source.to_owned(),
        groups: Vec::new(),
        links: Vec::new(),
        callees: Vec::new(),
    };
    let Ok(tree) = syn::parse_file(source) else {
        return draft;
    };
    let text = Text::new(source, &tree);
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
        krate,
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
        fields: HashMap::new(),
        impls: Vec::new(),
        owners: Vec::new(),
        functions: Vec::new(),
        links: Vec::new(),
        declared: HashMap::new(),
        callees: Vec::new(),
        callee_names: HashMap::new(),
        signatures: HashMap::new(),
        returns: Vec::new(),
    };
    walker.candidates(&tree);
    walker.callee_candidates(&tree.items);
    walker.visit_file(&tree);
    let Walker {
        vars,
        mut groups,
        pending,
        awaiting,
        links,
        callees,
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
    draft.groups = groups;
    draft.links = links;
    draft.callees = callees
        .into_iter()
        .map(|callee| {
            let groups = callee.groups().collect();
            (callee.name, callee.calls, groups)
        })
        .collect();
    draft
}

/// Finishes every file's plan in one package: a field is rewritten only if
/// nothing in the package can reach it in a way fix can't follow.
pub(super) fn settle(mut drafts: Vec<Draft>, krate: &Crate) -> Vec<(PathBuf, Outcome)> {
    for draft in &mut drafts {
        draft.share();
    }
    // The statements that would become actor callbacks, anywhere in the
    // package, and the names they call.
    let callbacks: BTreeSet<String> = drafts
        .iter()
        .flat_map(|draft| &draft.groups)
        .filter(|group| group.refusal.is_none())
        .flat_map(|group| group.callbacks.iter().cloned())
        .collect();
    for draft in &mut drafts {
        draft.reach(krate, &callbacks);
        draft.share();
    }
    drafts.into_iter().map(Draft::finish).collect()
}

impl Draft {
    /// The groups that share a state, through the links between them.
    fn components(&self) -> Vec<Vec<usize>> {
        let mut parent: Vec<usize> = (0..self.groups.len()).collect();
        fn root(parent: &mut [usize], mut node: usize) -> usize {
            while let Some(&up) = parent.get(node)
                && up != node
            {
                node = up;
            }
            node
        }
        for &(a, b) in &self.links {
            let (a, b) = (root(&mut parent, a), root(&mut parent, b));
            if let Some(slot) = parent.get_mut(a) {
                *slot = b;
            }
        }
        let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for group in 0..self.groups.len() {
            components
                .entry(root(&mut parent, group))
                .or_default()
                .push(group);
        }
        components.into_values().collect()
    }

    /// Refuses every group that holds the state of a refused one.
    fn share(&mut self) {
        for component in self.components() {
            let refused = component.iter().any(|&group| {
                self.groups
                    .get(group)
                    .is_some_and(|group| group.refusal.is_some())
            });
            if refused {
                for &group in &component {
                    if let Some(group) = self.groups.get_mut(group) {
                        group.refuse(SHARED);
                    }
                }
            }
        }
    }

    /// Refuses each state that the package might reach where fix can't
    /// follow, with every group holding it. A local is reached through the
    /// functions that lock it as a field is, since a caller of one of them
    /// might run inside an actor's callback.
    fn reach(&mut self, krate: &Crate, callbacks: &BTreeSet<String>) {
        for group in &mut self.groups {
            if let Some(reason) = krate.unreadable_reason() {
                group.refuse(Reason::Unfollowed(reason));
            } else if let Some(member) = &group.member
                && let Some(reason) = Self::unseen(krate, member, &self.file)
            {
                group.refuse(reason);
            }
        }
        // Every mention of a function's name but its own must be a call fix
        // followed, or code fix can't see might pass or take other values.
        for (name, calls, groups) in &self.callees {
            if krate.occurrences(name) != calls + 1 {
                for &group in groups {
                    if let Some(group) = self.groups.get_mut(group) {
                        group.refuse(Reason::Unfollowed(
                            "its function is named somewhere fix doesn't follow as a call",
                        ));
                    }
                }
            }
        }
        for component in self.components() {
            let groups: Vec<&Group> = component
                .iter()
                .filter_map(|&group| self.groups.get(group))
                .collect();
            if groups.iter().any(|group| group.refusal.is_some()) {
                continue;
            }
            let locking = groups
                .iter()
                .flat_map(|g| g.functions.iter().cloned())
                .collect();
            let blocks = groups
                .iter()
                .flat_map(|g| g.poll_blocks.iter().cloned())
                .collect();
            let boundary = if groups.iter().any(|group| group.escapes) {
                None
            } else {
                krate.poll_hazard(&krate.callers(&locking), &blocks)
            };
            let reason = if boundary.is_some() {
                Some(Reason::Poll)
            } else {
                Self::hazard(&groups, krate, callbacks)
            };
            if let Some(reason) = reason
                && let Some(group) = component
                    .first()
                    .and_then(|&first| self.groups.get_mut(first))
            {
                group.refuse(reason);
                group.boundary = boundary;
            }
        }
    }

    /// Why the package might use the field `member` where fix can't see.
    fn unseen(krate: &Crate, member: &str, file: &Path) -> Option<Reason> {
        if krate.field_declarations(member) > 1 {
            return Some(Reason::Unfollowed(
                "another struct, union or variant in its package has a field of its name",
            ));
        }
        krate
            .names_member_elsewhere(member, file)
            .then_some(Reason::Unfollowed(
                "another file in its package names a field of its name",
            ))
    }

    /// Why code that locks the state of `groups` might run inside an actor's
    /// callback, if it might.
    fn hazard(groups: &[&Group], krate: &Crate, callbacks: &BTreeSet<String>) -> Option<Reason> {
        if groups.iter().any(|group| group.escapes) {
            return Some(Reason::Nested(
                "it is locked in a closure or an async block, which might run inside an actor's callback",
            ));
        }
        let locking: BTreeSet<String> = groups
            .iter()
            .flat_map(|group| group.functions.iter().cloned())
            .collect();
        let reached = krate.callers(&locking);
        match krate.hazard(&reached) {
            Some(Hazard::Trait) => {
                return Some(Reason::Nested(
                    "it is locked through a trait method, which might run inside an actor's callback",
                ));
            }
            Some(Hazard::Public) => {
                return Some(Reason::Nested(
                    "it is locked through a public function, which another crate might call inside an actor's callback",
                ));
            }
            Some(Hazard::Escapes) => {
                return Some(Reason::Nested(
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback",
                ));
            }
            Some(Hazard::Callbacks) => {
                return Some(Reason::Nested(
                    "an actor callback in its package calls code that might lock it",
                ));
            }
            None => {}
        }
        (!reached.is_disjoint(callbacks)).then_some(Reason::Nested(
            "a function that locks it is called in a statement that becomes an actor's callback",
        ))
    }

    fn finish(self) -> (PathBuf, Outcome) {
        let mut outcome = Outcome {
            edits: Vec::new(),
            refusals: Vec::new(),
        };
        for group in self.groups {
            match group.refusal {
                Some(reason) => outcome.refusals.push(Refusal {
                    line: group.line,
                    name: group.name,
                    reason,
                    boundary: group.boundary,
                }),
                None => outcome.edits.extend(group.edits),
            }
        }
        // A module in another file can name its ancestors' private imports, so
        // fix can't see every use of them.
        if !outcome.edits.is_empty()
            && let Ok(tree) = syn::parse_file(&self.source)
            && !has_out_of_line_module(&tree)
        {
            let text = Text::new(&self.source, &tree);
            let imports = Imports::prune(&text, &self.file, &tree, &outcome.edits);
            outcome.edits.extend(imports);
        }
        outcome.edits = coalesce(outcome.edits);
        (self.file, outcome)
    }
}

/// Whether any module in the file is declared as `mod name;`.
fn has_out_of_line_module(tree: &syn::File) -> bool {
    struct Modules(bool);
    impl<'ast> Visit<'ast> for Modules {
        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            self.0 |= module.content.is_none();
            syn::visit::visit_item_mod(self, module);
        }
    }
    let mut modules = Modules(false);
    modules.visit_file(tree);
    modules.0
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

impl<'a> Text<'a> {
    fn new(source: &'a str, tree: &syn::File) -> Self {
        Self {
            source,
            prefix: usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
                + tree.shebang.as_ref().map_or(0, String::len),
        }
    }

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
    /// Every name an item or import binds anywhere in the file. A relative
    /// path through one of these, as `std` after `use custom as std;`, may
    /// not reach the crate of that name.
    bound: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Names {
    fn visit_item(&mut self, item: &'ast Item) {
        self.bound
            .extend(ItemScope::bindings(item).into_iter().map(|(name, _)| name));
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
pub(super) fn known_macro(name: &str) -> Option<usize> {
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
    /// Whether the first argument is a condition, as an assertion's is,
    /// rather than a value the macro formats.
    condition: bool,
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
                condition: name == "assert" || name == "debug_assert",
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
    /// A generic parameter.
    Parameter,
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
                // A leading `::` names a crate, never a local module.
                let mut prefix = item
                    .leading_colon
                    .map(|_| vec!["::".to_owned()])
                    .unwrap_or_default();
                own.import(&item.tree, &mut prefix);
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

pub(super) fn item_attributes(item: &Item) -> &[syn::Attribute] {
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
    Parameter,
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
    /// Whether this stands for a struct field rather than a local. Its name
    /// is the field's, and an access through a traced receiver uses it.
    field: bool,
    /// Whether this is a `&Mutex<T>` parameter or a copy of one, whose
    /// `clone` copies the reference where an actor's copies the actor.
    mutex_ref: bool,
    /// The struct with candidate fields that this variable holds or points
    /// to, by its declared type or a struct literal.
    record: Option<String>,
}

/// One `Arc<Mutex<T>>` and every variable that holds a clone of it.
struct Group {
    name: String,
    line: usize,
    refusal: Option<Reason>,
    edits: Vec<Edit>,
    /// The field's name, for a struct field.
    member: Option<String>,
    /// The functions whose bodies take a guard of it.
    functions: BTreeSet<String>,
    poll_blocks: BTreeSet<String>,
    boundary: Option<String>,
    /// Whether a guard of it is taken in a closure, an async block or
    /// outside any function.
    escapes: bool,
    /// The names of the functions and methods its callbacks might run.
    callbacks: BTreeSet<String>,
}

impl Group {
    fn new(name: String, line: usize, edits: Vec<Edit>) -> Self {
        Self {
            name,
            line,
            refusal: None,
            edits,
            member: None,
            functions: BTreeSet::new(),
            poll_blocks: BTreeSet::new(),
            boundary: None,
            escapes: false,
            callbacks: BTreeSet::new(),
        }
    }

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
    krate: &'a Crate,
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
    /// The struct fields that could become actors, by name.
    fields: HashMap<String, fields::Field>,
    /// For each enclosing `impl`, the struct with candidate fields it is
    /// for, if it is for one.
    impls: Vec<Option<String>>,
    /// For each enclosing `impl`, the type whose methods `self` calls, if
    /// fix can name it; `None` for a function or trait, where `self` and
    /// `Self` mean something else.
    owners: Vec<Option<String>>,
    /// The enclosing functions, with the stack depth where each body starts.
    functions: Vec<(String, usize)>,
    /// Pairs of groups that hold the same state.
    links: Vec<(usize, usize)>,
    /// How many items and imports in the file bind each name.
    declared: HashMap<String, usize>,
    /// The private functions with parameters or a return value to rewrite.
    callees: Vec<calls::Callee>,
    /// Each candidate function by name, or `None` when two have one name.
    callee_names: HashMap<String, Option<usize>>,
    /// Each candidate function by where its name is declared.
    signatures: HashMap<usize, usize>,
    /// For each enclosing function, closure and async block, the group of
    /// the value it returns, when it is a function that returns one.
    returns: Vec<Option<calls::Frame<'a>>>,
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
            field: false,
            mutex_ref: false,
            record: None,
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.vars.push(index);
        }
        index
    }

    fn declare_pattern(&mut self, pat: &Pat) {
        self.destructures(pat);
        let mut names = Vec::new();
        pattern_names(pat, &mut names);
        for name in names {
            self.declare(name, None);
        }
        if let Pat::Type(typed) = pat {
            self.record(&typed.pat, &typed.ty);
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

    /// A tracked variable named by `expr`, or the variable for a candidate
    /// field that `expr` reads through a traced receiver.
    fn tracked(&self, expr: &Expr) -> Option<usize> {
        self.field_place(expr)
            .or_else(|| self.local(expr).filter(|&var| self.group_of(var).is_some()))
    }

    /// The text that names `var` where `expr` does: a local's spelling at
    /// its declaration, or a field access as written.
    fn holder(&self, var: usize, expr: &Expr) -> Option<String> {
        let var = self.vars.get(var)?;
        if var.field {
            Some(self.text.get(self.text.of(expr)?)?.to_owned())
        } else {
            Some(var.spelling.clone())
        }
    }

    fn resolve_name(&self, name: &str) -> Resolved {
        let mut scopes = self.items.iter().rev();
        while let Some(scope) = scopes.next() {
            if let Some(binding) = scope.names.get(name) {
                return match binding {
                    Binding::Import(path) => Resolved::Path(path.clone()),
                    Binding::Item => Resolved::Item,
                    Binding::Parameter => Resolved::Parameter,
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
                                Some(Binding::Parameter) => Resolved::Parameter,
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

    /// Whether `path` without its last `skip` segments is `std::sync::Arc`.
    fn is_arc(&self, path: &syn::Path, skip: usize) -> bool {
        let Some(count) = path.segments.len().checked_sub(skip) else {
            return false;
        };
        self.resolve_path(path, count)
            .and_then(|resolved| self.crate_path(resolved))
            .is_some_and(|resolved| {
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

    /// A resolved path as one starting from a crate, or `None` when its
    /// first name might be a local alias rather than a crate.
    fn crate_path(&self, resolved: Vec<String>) -> Option<Vec<String>> {
        if resolved.first().is_some_and(|first| first == "::") {
            return Some(resolved.into_iter().skip(1).collect());
        }
        (!resolved
            .first()
            .is_some_and(|first| self.names.bound.contains(first)))
        .then_some(resolved)
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
        self.actor_of(self.arc_state(ty)?)
    }

    /// `::adamantium::Actor<T>` for the state `T`.
    fn actor_of(&self, state: &Type) -> Option<String> {
        Some(format!("{ACTOR}<{}>", self.text.get(self.text.of(state)?)?))
    }

    /// `T` in `Arc<Mutex<T>>`.
    fn arc_state<'t>(&self, ty: &'t Type) -> Option<&'t Type> {
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
        let Some(GenericArgument::Type(mutex)) = args.args.first() else {
            return None;
        };
        self.mutex_state(mutex)
    }

    /// `T` in a `Mutex<T>` that Clippy resolved to `std::sync::Mutex`.
    fn mutex_state<'t>(&self, ty: &'t Type) -> Option<&'t Type> {
        let Type::Path(mutex) = ty else {
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
        Some(state)
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

    /// The variable cloned or moved by `Arc::clone(&m)`, `Arc::clone(r)`
    /// for a reference `r`, `m.clone()` or `m`, with the replacement for the
    /// first two forms.
    fn clone_of(&self, expr: &Expr) -> Option<(usize, Option<String>)> {
        match expr {
            Expr::Path(_) => self.tracked(expr).map(|var| (var, None)),
            Expr::MethodCall(call)
                if call.method == "clone" && call.args.is_empty() && call.turbofish.is_none() =>
            {
                self.tracked(&call.receiver)
                    .filter(|&var| !self.vars.get(var).is_some_and(|var| var.mutex_ref))
                    .map(|var| (var, None))
            }
            Expr::Call(call) => {
                let Expr::Path(func) = &*call.func else {
                    return None;
                };
                if func.qself.is_some()
                    || call.args.len() != 1
                    || func.path.segments.last()?.ident != "clone"
                    || !self.is_arc(&func.path, 1)
                {
                    return None;
                }
                // A variable passed as it is compiles only as a reference,
                // a parameter or a copy of one.
                let held = match call.args.first()? {
                    Expr::Reference(reference) if reference.mutability.is_none() => {
                        &*reference.expr
                    }
                    held @ Expr::Path(_) => held,
                    _ => return None,
                };
                let var = self.tracked(held)?;
                let name = self.holder(var, held)?;
                Some((var, Some(format!("{name}.clone()"))))
            }
            _ => None,
        }
    }

    fn local_statement(&mut self, local: &'a Local) {
        self.destructures(&local.pat);
        let mut names = Vec::new();
        pattern_names(&local.pat, &mut names);
        // A conditional `let` may shadow a binding in one configuration and
        // not in another, whatever it declares.
        if !local.attrs.is_empty() {
            for name in &names {
                if let Some(var) = self.lookup(name) {
                    self.refuse(
                        var,
                        Reason::Unfollowed("a conditional declaration shadows it"),
                    );
                }
            }
        }
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
                let edits = std::iter::once(self.edit(range, constructor))
                    .chain(ty_edit)
                    .collect();
                self.bind(local, name, spelling, edits);
                return;
            }
            if let Some(ty_edit) = ty_edit.clone()
                && let Some(returned) = self.returned_by(init)
            {
                let var = self.bind(local, name, spelling, ty_edit.into_iter().collect());
                if let Some(group) = self.group_of(var) {
                    self.links.push((group, returned));
                }
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
                // A copy of a `&Mutex<T>` is one too.
                let mutex_ref = matches!(init, Expr::Path(_))
                    && self.vars.get(source).is_some_and(|var| var.mutex_ref);
                let var = self.declare(name, group);
                if let Some(entry) = self.vars.get_mut(var) {
                    entry.spelling = spelling;
                    entry.mutex_ref = mutex_ref;
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
        let mut declared = None;
        for name in names {
            declared = Some(self.declare(name, None));
        }
        if let (Some(_), Some(var)) = (ident, declared) {
            let record = match (ty, init) {
                (Some(ty), _) => self.record_type(ty),
                (None, Some(Expr::Struct(literal))) if literal.qself.is_none() => {
                    self.literal_struct(&literal.path)
                }
                _ => None,
            };
            if let Some(entry) = self.vars.get_mut(var) {
                entry.record = record;
            }
        }
        self.settle(local, ident.and(declared));
    }

    /// Declares the local that `local` binds as a new state, with `edits`.
    fn bind(
        &mut self,
        local: &'a Local,
        name: String,
        spelling: String,
        edits: Vec<Edit>,
    ) -> usize {
        let group = self.groups.len();
        let mut state = Group::new(name.clone(), self.text.line(local.let_token.span), edits);
        if !local.attrs.is_empty() {
            state.refuse(Reason::Unfollowed("its declaration has attributes"));
        }
        if self.names.constants.contains(&name) {
            state.refuse(Reason::Unfollowed(
                "a constant, static, struct or import in the file has its name",
            ));
        }
        self.groups.push(state);
        let var = self.declare(name, Some(group));
        if let Some(entry) = self.vars.get_mut(var) {
            entry.spelling = spelling;
        }
        self.settle(local, Some(var));
        var
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
pub(super) fn pattern_names(pat: &Pat, names: &mut Vec<String>) {
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
        let held = call.args.first()?;
        let var = self.tracked(held)?;
        let start = self.text.range(first.method.span())?.start;
        let end = self.text.range(first.paren_token.span.close())?.end;
        let unwrap = self.text.get(start..end)?;
        let name = self.holder(var, held)?;
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
                .and_then(|path| self.crate_path(path))
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
                        // A braced macro with no semicolon at the end of a
                        // block is the block's value, not a statement.
                        let tail = mac.semi_token.is_none()
                            && matches!(
                                index.checked_sub(1).and_then(|below| self.stack.get(below)),
                                Some(Node::Block(block, _))
                                    if block.stmts.last().is_some_and(|last| std::ptr::eq(last, stmt))
                            );
                        if tail {
                            return Err(Reason::Unfollowed(
                                "a guard is in a macro whose value ends a block",
                            ));
                        }
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
            // An operand or index that reads a `&mut` through the guard
            // reborrows it rather than copying it out, so other reads stay
            // in the statement's `with`.
            _ => None,
        };
        let read = projected
            && match parent {
                Some(Node::Expr(expr)) => {
                    matches!(expr, Expr::Binary(binary) if is_comparison(&binary.op))
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
            Ok((edit, runs)) => {
                self.lock_site(var, runs);
                self.add_edit(var, edit);
            }
            Err(reason) => self.refuse(var, reason),
        }
    }

    /// The edit that turns `guard`'s statement into an actor call, and the
    /// names of the functions and methods its callback might run.
    fn guard_edit(
        &mut self,
        var: usize,
        guard: &'a Expr,
    ) -> Result<(Edit, BTreeSet<String>), Reason> {
        if self.inside_actor() {
            return Err(Reason::InsideActor);
        }
        let plan = self.plan(guard)?;
        let (bare_name, name) = self
            .vars
            .get(var)
            .map(|var| (var.name.clone(), var.spelling.clone()))
            .ok_or(UNLOCATED)?;
        let held = lock_receiver(guard).ok_or(UNLOCATED)?;
        let holder = self.holder(var, held).ok_or(UNLOCATED)?;
        // A field's receiver stays outside the closure, so the code moving
        // into it must not use the receiver either.
        let base = self.place_base(held);
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
        if inside.calls {
            return Err(Reason::Calls);
        }
        let Scan { runs, .. } = inside;
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
            if names_in(region, &bare_name) != Some(1)
                || base
                    .as_ref()
                    .is_some_and(|base| names_in(region, base) != Some(1))
            {
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
            let replacement = format!("{holder}.read(|{name}| {})", substitute(range.clone())?);
            return Ok((self.edit(range, replacement), runs));
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
                    format!("{holder}.{method}(|{name}| {body})")
                } else {
                    format!("{holder}.{method}(|{name}| {{ {body}; }})")
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
                (range, format!("{holder}.{method}(|{name}| {body})"))
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
                (range, format!("{holder}.{method}(|{name}| {body})"))
            }
        };
        Ok((self.edit(range, replacement), runs))
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
        let callee = self.signature(sig);
        let inputs = sig.inputs.iter().filter_map(|input| match input {
            syn::FnArg::Typed(typed) => Some(typed),
            syn::FnArg::Receiver(_) => None,
        });
        for (position, typed) in inputs.enumerate() {
            self.declare_pattern(&typed.pat);
            self.record(&typed.pat, &typed.ty);
            if let Some(callee) = callee {
                self.parameter(callee, position, &typed.pat);
            }
        }
        let frame = callee.and_then(|callee| self.frame(callee, body));
        self.returns.push(frame);
        self.functions.push((bare(&sig.ident), self.stack.len()));
        self.generic(&sig.generics, |walker| walker.block(body, BlockKind::Body));
        self.functions.pop();
        self.returns.pop();
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
            scope.names.insert(name.to_string(), Binding::Parameter);
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
                    let name = bare(&ident);
                    self.mention(&name, Reason::Unfollowed("it is used in a macro"));
                    self.mention_field(&name, Reason::Unfollowed("it is used in a macro"));
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

/// `m` in the guard `m.lock().unwrap()`.
fn lock_receiver(guard: &Expr) -> Option<&Expr> {
    let Expr::MethodCall(unwrap) = guard else {
        return None;
    };
    let Expr::MethodCall(lock) = &*unwrap.receiver else {
        return None;
    };
    Some(&lock.receiver)
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
        if macro_attribute(item_attributes(item))
            && !matches!(item, Item::Fn(f) if self.krate.poll_root(self.file, &f.sig))
        {
            self.attribute_macro(item.to_token_stream());
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
            Item::Fn(function) => {
                self.impls.push(None);
                self.owners.push(None);
                self.function(&function.sig, &function.block);
                self.owners.pop();
                self.impls.pop();
            }
            Item::Impl(item) => self.generic(&item.generics, |walker| {
                let structure = walker.impl_struct(item);
                let owner = walker.owner(item);
                walker.impls.push(structure);
                walker.owners.push(owner);
                syn::visit::visit_item_impl(walker, item);
                walker.owners.pop();
                walker.impls.pop();
            }),
            Item::Trait(item) => self.generic(&item.generics, |walker| {
                walker.impls.push(None);
                walker.owners.push(None);
                syn::visit::visit_item_trait(walker, item);
                walker.owners.pop();
                walker.impls.pop();
            }),
            _ => syn::visit::visit_item(self, item),
        }
        self.scopes.pop();
    }

    fn visit_impl_item_fn(&mut self, function: &'a syn::ImplItemFn) {
        if macro_attribute(&function.attrs) {
            self.attribute_macro(function.to_token_stream());
        }
        self.function(&function.sig, &function.block);
    }

    fn visit_trait_item_fn(&mut self, function: &'a syn::TraitItemFn) {
        if macro_attribute(&function.attrs) {
            self.attribute_macro(function.to_token_stream());
        }
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
        if let Some(group) = self.tail(expr) {
            self.returned(group, expr);
            return;
        }
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
        if let Expr::Assign(assign) = expr {
            self.assigns(&assign.left);
        }
        self.stack.push(Node::Expr(expr));
        if self.followed_call(expr) || self.followed_return(expr) {
            self.stack.pop();
            return;
        }
        match expr {
            Expr::Struct(literal) => self.literal(literal),
            Expr::Field(field) if self.candidate(&field.member).is_some() => {
                self.stray(field);
                syn::visit::visit_expr(self, expr);
            }
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
            // A closure's or an async block's `return` is its own.
            Expr::Closure(closure) => {
                self.returns.push(None);
                self.scoped(|walker| {
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
                });
                self.returns.pop();
            }
            Expr::Async(block) => {
                self.returns.push(None);
                self.block(&block.block, BlockKind::Body);
                self.returns.pop();
            }
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
    calls: bool,
    unfollowed: Option<&'static str>,
    /// The names of the functions and methods the code might run: method
    /// names and every path that doesn't name a local. A field or a local
    /// doesn't run anything.
    runs: BTreeSet<String>,
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
            calls: false,
            unfollowed: None,
            runs: BTreeSet::new(),
        }
    }

    fn full(&mut self, full: Full<'a>) {
        match full {
            Full::Expr(expr) => self.visit_expr(expr),
            Full::Macro(mac) => self.visit_macro(mac),
        }
    }

    /// Whether `expr` is the state or a value computed from it alone: the
    /// guard reached through fields, indexes, unary operators, borrows,
    /// casts and method calls, or a macro, such as `write!`, given the guard.
    fn computed_from_state(&self, mut expr: &Expr) -> bool {
        loop {
            if is(expr, self.guard) {
                return true;
            }
            expr =
                match expr {
                    Expr::Field(field) => &field.base,
                    Expr::Index(index) => &index.expr,
                    Expr::Unary(unary) => &unary.expr,
                    Expr::Reference(reference) => &reference.expr,
                    Expr::Paren(paren) => &paren.expr,
                    Expr::Cast(cast) => &cast.expr,
                    Expr::MethodCall(call) => &call.receiver,
                    Expr::Macro(_) => {
                        let text = &self.walker.text;
                        return text.of(expr).zip(text.of(self.guard)).is_some_and(
                            |(outer, guard)| outer.start <= guard.start && guard.end <= outer.end,
                        );
                    }
                    _ => return false,
                };
        }
    }
}

impl<'a> Visit<'a> for Scan<'_, 'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if is(expr, self.guard) {
            return;
        }
        match expr {
            Expr::MethodCall(call) => {
                self.runs.insert(bare(&call.method));
            }
            Expr::Path(path) if self.walker.local(expr).is_none() => {
                self.runs.extend(
                    path.path
                        .segments
                        .iter()
                        .map(|segment| bare(&segment.ident)),
                );
            }
            _ => {}
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
            // An actor's callback can't use any actor, and a function or a
            // method on another value might. Only methods on the state, or
            // on a value computed from it, are called under the guard.
            Expr::Call(_) => self.calls = true,
            Expr::MethodCall(call) if !self.computed_from_state(&call.receiver) => {
                self.calls = true
            }
            // Indexing, field access (through a custom `Deref`) and unary
            // operators on another value call its trait implementations, and
            // assigning to another place drops its old value.
            Expr::Index(index) if !self.computed_from_state(&index.expr) => self.calls = true,
            Expr::Field(field) if !self.computed_from_state(&field.base) => self.calls = true,
            Expr::Unary(unary)
                if !matches!(&*unary.expr, Expr::Lit(_))
                    && !self.computed_from_state(&unary.expr) =>
            {
                self.calls = true;
            }
            Expr::Assign(assign) if !self.computed_from_state(&assign.left) => self.calls = true,
            // An operator calls its operands' trait implementation, such as
            // `PartialEq` or `AddAssign`, which might use an actor too, unless
            // both operands are the state's or literals.
            Expr::Binary(binary)
                if !matches!(binary.op, BinOp::And(_) | BinOp::Or(_))
                    && [&binary.left, &binary.right].into_iter().any(|operand| {
                        !matches!(&**operand, Expr::Lit(_)) && !self.computed_from_state(operand)
                    }) =>
            {
                self.calls = true;
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
                    let value = parsed.named(position, arg).unwrap_or(arg);
                    // Formatting a value calls its `Display` or `Debug`, and
                    // `write!` calls its destination's `write_fmt`, which
                    // might use an actor like any other call.
                    let calls = if position == parsed.format {
                        match value {
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(format),
                                ..
                            }) => !format_captures(&format.value()).is_empty(),
                            _ => true,
                        }
                    } else {
                        !(position == 0 && parsed.condition)
                            && !matches!(value, Expr::Lit(_))
                            && !self.computed_from_state(value)
                    };
                    self.calls |= calls;
                    let conditional = usize::from(position >= parsed.conditional);
                    self.branch += conditional;
                    self.visit_expr(value);
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

    /// A `let` drops its value at the end of the block, inside the callback,
    /// and a `Drop` might use an actor.
    fn visit_local(&mut self, local: &'a Local) {
        if self.deferred == 0 {
            self.calls = true;
        }
        syn::visit::visit_local(self, local);
    }
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
                        self.idents.entry(bare(&ident)).or_default().push(range);
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
            self.idents.entry(bare(ident)).or_default().push(range);
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
                prefix.push(bare(&path.ident));
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
                _ => bare(name),
            }
        } else {
            path.push(bare(ident));
            bare(name)
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

    /// Plans `source` as the only file of its package, as Clippy would see
    /// it with every occurrence of each of `mutexes` resolved to
    /// `std::sync::Mutex`.
    pub(super) fn outcome(source: &str, mutexes: &[&str]) -> Outcome {
        let mut spans = BTreeSet::new();
        for mutex in mutexes {
            for (start, _) in source.match_indices(mutex) {
                spans.insert((start, start + mutex.len()));
            }
        }
        let file = Path::new("lib.rs");
        let mut krate = Crate::default();
        krate.add(file, source);
        let mut settled = settle(vec![plan(file, source, &spans, &krate)], &krate);
        settled.pop().map(|(_, outcome)| outcome).unwrap()
    }

    /// Plans `source` and applies the edits.
    pub(super) fn fix(source: &str, mutexes: &[&str]) -> (String, Vec<(String, Reason)>) {
        let outcome = outcome(source, mutexes);
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
            "fn f() -> Option<Arc<Mutex<i32>>> { let m = Arc::new(Mutex::new(0)); Some(m) }",
            "fn f() { let m = Arc::new(Mutex::new(0)); println!(\"{m:?}\"); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); other!(m); }",
            "fn f() { let m = Arc::new(Mutex::new(0)); let h = m.clone(); *m.lock().unwrap() += 1; consume(h); }",
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
        assert_eq!(outcome(source, &["Mutex"]).edits.len(), 2);
        let (fixed, _) = fix(source, &["Mutex"]);
        assert_eq!(
            fixed,
            "fn f() {\n    let m = ::adamantium::Actor::new(1);\n    drop(m);\n}\n"
        );
    }

    #[test]
    fn a_statement_naming_the_binding_again_is_refused() {
        for body in [
            "fn f() { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push({ &m; 1 }); }",
            "fn f() { let m = Arc::new(Mutex::new(Vec::new())); m.lock().unwrap().push({ || &m; 1 }); }",
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
    fn locals_whose_function_might_run_inside_an_actor_callback_are_refused() {
        const COUNT: &str =
            "fn count() { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }";
        for (body, reason) in [
            (
                format!("{COUNT}\nfn f(a: &adamantium::Actor<u8>) {{ a.with(|_| count()); }}"),
                Reason::Nested("an actor callback in its package calls code that might lock it"),
            ),
            (
                format!("{COUNT}\nfn all() {{ [1].iter().for_each(|_| count()); }}"),
                Reason::Nested(
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback",
                ),
            ),
            (
                "fn f() { let m = Arc::new(Mutex::new(0)); let bump = || *m.lock().unwrap() += 1; bump(); }".to_owned(),
                Reason::Nested(
                    "it is locked in a closure or an async block, which might run inside an actor's callback",
                ),
            ),
            (
                "async fn f() { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }"
                    .to_owned(),
                Reason::Poll,
            ),
            (
                "trait Bump { fn bump(&self) { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; } }".to_owned(),
                Reason::Nested(
                    "it is locked through a trait method, which might run inside an actor's callback",
                ),
            ),
        ] {
            refused(&body, "m", reason);
        }
    }

    #[test]
    fn a_local_locked_by_a_method_another_callback_calls_is_refused() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};
struct S;
impl S { fn bump(&self) { count(); } }
fn count() { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }
fn f() { let s = Arc::new(Mutex::new(S)); s.lock().unwrap().bump(); }
",
            &["Mutex"],
        );
        assert_eq!(
            refusals,
            [(
                "m".to_owned(),
                Reason::Nested(
                    "a function that locks it is called in a statement that becomes an actor's callback"
                )
            )]
        );
        assert!(fixed.contains("s.with(|s| { (*s).bump(); });"), "{fixed}");
    }

    #[test]
    fn a_local_locked_by_a_function_imported_under_another_name_is_refused() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nmod inner {\n    use std::sync::{Arc, Mutex};\n    pub(crate) fn count() { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }\n}\nuse inner::count as bump;\nstruct S;\nimpl S { fn go(&self) { bump(); } }\nfn f() { let s = Arc::new(Mutex::new(S)); s.lock().unwrap().go(); }\n",
            &["Mutex"],
        );
        assert_eq!(
            refusals,
            [(
                "m".to_owned(),
                Reason::Nested(
                    "a function that locks it is called in a statement that becomes an actor's callback"
                )
            )]
        );
        assert!(fixed.contains("s.with(|s| { (*s).go(); });"), "{fixed}");
    }

    #[test]
    fn a_local_named_like_its_function_is_rewritten() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};
fn hits() -> u32 {
    let hits = Arc::new(Mutex::new(0u32));
    *hits.lock().unwrap() += 1;
    let value = *hits.lock().unwrap();
    value
}
",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("hits.with(|hits| *hits += 1);"), "{fixed}");
    }

    #[test]
    fn coercion_site_operands_are_reborrowed_not_copied() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct S<'a> { x: &'a mut i32 }\nfn f(mut x: i32) {\n    let m = Arc::new(Mutex::new(&mut x));\n    S { x: *m.lock().unwrap() };\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("m.with(|m| { S { x: *m }; });"), "{fixed}");
    }

    #[test]
    fn calls_that_might_use_an_actor_are_refused() {
        for body in [
            "fn other() -> u32 { 1 }\nfn f() { let m = Arc::new(Mutex::new(0u32)); *m.lock().unwrap() += other(); }",
            "fn f() { let m = Arc::new(Mutex::new(0u32)); let other = || 1; *m.lock().unwrap() += other(); }",
            "fn bump(x: &mut u32) { *x += 1; }\nfn f() { let m = Arc::new(Mutex::new(0u32)); bump(&mut *m.lock().unwrap()); }",
        ] {
            refused(body, "m", Reason::Calls);
        }
        for body in [
            "fn f(other: &[u32]) { let m = Arc::new(Mutex::new(0usize)); *m.lock().unwrap() += other.len(); }",
            "fn f(other: &[u32]) { let m = Arc::new(Mutex::new(0usize)); *m.lock().unwrap() += (other, 1).0.len(); }",
            "fn f(other: &[u32]) { let m = Arc::new(Mutex::new(0usize)); let _n = (other, m.lock().unwrap().len()).0.len(); }",
            // Formatting calls `Display` or `Debug`, and `write!` calls the
            // destination's `write_fmt`.
            "fn f(other: &str) { let m = Arc::new(Mutex::new(0u32)); let _s = format!(\"{} {}\", *m.lock().unwrap(), other); }",
            "fn f(other: &str) { let m = Arc::new(Mutex::new(0u32)); let _s = format!(\"{} {other}\", *m.lock().unwrap()); }",
            "const OTHER: &str = \"\";\nfn f() { let m = Arc::new(Mutex::new(0u32)); assert_eq!(*m.lock().unwrap(), 0, \"{}\", OTHER); }",
            "fn f(other: &str) { let m = Arc::new(Mutex::new(\"\")); assert_eq!(*m.lock().unwrap(), other); }",
            "fn f(out: &mut String) { use std::fmt::Write; let m = Arc::new(Mutex::new(0u32)); writeln!(out, \"{}\", *m.lock().unwrap()).unwrap(); }",
        ] {
            refused(body, "m", Reason::Calls);
        }
        // Without types, an operand read through the guard might be a `&mut`
        // that the operator reborrows, so the whole statement stays in the
        // callback, call and all.
        refused(
            "fn other() -> u32 { 1 }\nfn f() -> u32 { let m = Arc::new(Mutex::new(0u32)); *m.lock().unwrap() + other() }",
            "m",
            Reason::Calls,
        );
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
            "use std::fmt::Write;\nuse std::sync::{Arc, Mutex};\nfn f() -> String {\n    let m = Arc::new(Mutex::new(String::new()));\n    write!(*m.lock().unwrap(), \"hello\").unwrap();\n    format!(\"{}\", *m.lock().unwrap())\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("m.with(|m| { write!(*m, \"hello\").unwrap(); });"),
            "{fixed}"
        );
        assert!(fixed.contains("m.read(|m| format!(\"{}\", *m))"), "{fixed}");
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
            "struct D;\nfn f(flag: bool, d: D) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().extend(if flag { [d] } else { [D] }); }",
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
    fn a_conditional_declaration_shadowing_a_binding_refuses_it() {
        for body in [
            "fn f() { let m = Arc::new(Mutex::new(0u32)); #[cfg(any())] let m = Arc::new(Mutex::new(1u32)); *m.lock().unwrap() += 1; }",
            "fn f() { let m = Arc::new(Mutex::new(0u32)); let h = m.clone(); #[cfg(any())] let m = h.clone(); *m.lock().unwrap() += 1; }",
        ] {
            refused(
                body,
                "m",
                Reason::Unfollowed("a conditional declaration shadows it"),
            );
        }
    }

    #[test]
    fn formatting_the_state_and_literals_is_rewritten() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() -> String {\n    let m = Arc::new(Mutex::new(1u32));\n    assert_eq!(*m.lock().unwrap(), 1, \"{}\", \"one\");\n    format!(\"{:>4} {}\", m.lock().unwrap().pow(2), 'x')\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("m.read(|m| { assert_eq!(*m, 1, \"{}\", \"one\"); });"),
            "{fixed}"
        );
        assert!(
            fixed.contains("m.with(|m| format!(\"{:>4} {}\", (*m).pow(2), 'x'))"),
            "{fixed}"
        );
    }

    #[test]
    fn a_clone_that_borrows_stays_inside_its_statement() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct S;\nimpl S { fn clone(&self) -> &Self { self } fn touch(&self) {} }\nfn f() {\n    let m = Arc::new(Mutex::new(S));\n    m.lock().unwrap().clone().touch();\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("m.with(|m| { (*m).clone().touch(); });"),
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
            "struct D;\nfn f(d: D) { let m = Arc::new(Mutex::new(true)); assert!(*m.lock().unwrap(), \"{}\", { let _d = d; \"bad\" }); }",
            "struct D;\nfn f(d: D) { let m = Arc::new(Mutex::new(1)); debug_assert_eq!(*m.lock().unwrap(), { let _d = d; 1 }); }",
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
    fn a_braced_macro_ending_a_block_keeps_its_value() {
        refused(
            "fn f() -> String { let m = Arc::new(Mutex::new(vec![1])); format! { \"{}\", m.lock().unwrap().len() } }",
            "m",
            Reason::Unfollowed("a guard is in a macro whose value ends a block"),
        );
    }

    #[test]
    fn imports_stay_when_a_child_module_is_in_another_file() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nmod child;\nfn f() {\n    let m = Arc::new(Mutex::new(1));\n    drop(m);\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.starts_with("use std::sync::{Arc, Mutex};\nmod child;\n"),
            "{fixed}"
        );
        assert!(fixed.contains("::adamantium::Actor::new(1)"), "{fixed}");
    }

    #[test]
    fn raw_identifiers_keep_their_imports() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() -> r#Arc<u8> {\n    let m = Arc::new(Mutex::new(0));\n    drop(m);\n    r#Arc::new(1)\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.starts_with("use std::sync::Arc;\n"), "{fixed}");
    }

    #[test]
    fn an_aliased_crate_root_is_not_the_standard_library() {
        for source in [
            "use custom as std;\nuse std::sync::Arc;\nuse ::std::sync::Mutex;\nfn f() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
            "extern crate custom as std;\nuse ::std::sync::Mutex;\nfn f() { let m = std::sync::Arc::new(Mutex::new(1)); drop(m); }\n",
        ] {
            let (fixed, refusals) = fix(source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert_eq!(refusals, []);
        }
        let (fixed, _) = fix(
            "use ::std::sync::{Arc, Mutex};\nmod std {}\nfn f() { let m = Arc::new(Mutex::new(1)); drop(m); }\n",
            &["Mutex"],
        );
        assert!(fixed.contains("::adamantium::Actor::new(1)"), "{fixed}");
    }

    #[test]
    fn operators_on_other_values_are_refused() {
        for body in [
            "struct View;\nimpl PartialEq<View> for i32 { fn eq(&self, _: &View) -> bool { true } }\nfn f(view: View) -> bool { let m = Arc::new(Mutex::new(1)); let same = *m.lock().unwrap() == view; same }",
            "fn f(n: u64) { let m = Arc::new(Mutex::new(0u64)); *m.lock().unwrap() += n; }",
        ] {
            refused(body, "m", Reason::Calls);
        }
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f() -> bool {\n    let m = Arc::new(Mutex::new(1));\n    *m.lock().unwrap() += 1;\n    *m.lock().unwrap() == 2\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("m.read(|m| *m == 2)"), "{fixed}");
    }

    #[test]
    fn implicit_calls_on_other_values_are_refused() {
        for body in [
            "fn f(view: &[u8]) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push(view[0]); }",
            "struct V { x: u8 }\nfn f(view: &V) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push(view.x); }",
            "fn f(view: &u8) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push(*view); }",
            "fn f(mut out: u8) { let m = Arc::new(Mutex::new(1u8)); out = *m.lock().unwrap(); drop(out); }",
            "fn f(view: u8) { let m = Arc::new(Mutex::new(vec![])); m.lock().unwrap().push({ let v = view; 1 }); }",
        ] {
            let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\n");
            let (fixed, refusals) = fix(&source, &["Mutex"]);
            assert_eq!(fixed, source);
            assert!(
                refusals.iter().any(|(name, _)| name == "m"),
                "{refusals:?}\n{source}"
            );
        }
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct S { items: Vec<u8> }\nfn f() -> u8 {\n    let m = Arc::new(Mutex::new(S { items: vec![1] }));\n    m.lock().unwrap().items[0] = 2;\n    -(m.lock().unwrap().items[0] as i8) as u8\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("m.with(|m| m.items[0] = 2);"), "{fixed}");
    }

    #[test]
    fn operator_and_index_operands_stay_in_with() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn f(mut n: i32) -> i32 {\n    let m = Arc::new(Mutex::new(&mut n));\n    let doubled = **m.lock().unwrap() * 2;\n    doubled\n}\n",
            &["Mutex"],
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("let doubled = m.with(|m| **m * 2);"),
            "{fixed}"
        );
        refused(
            "struct S;\nimpl std::ops::Add<&mut i32> for S { type Output = i32; fn add(self, x: &mut i32) -> i32 { *x } }\nfn f(mut n: i32) -> i32 { let m = Arc::new(Mutex::new(&mut n)); let x = S + *m.lock().unwrap(); x }",
            "m",
            Reason::Calls,
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
