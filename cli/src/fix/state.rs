use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
};

use proc_macro2::{TokenStream, TokenTree};
use syn::{
    BinOp, Block, Expr, ExprLit, GenericArgument, Item, Lit, Local, Macro, Pat, PathArguments,
    Stmt, Type, UnOp, UseTree, punctuated::Punctuated, visit::Visit,
};

use quote::ToTokens;

use super::{
    Edit,
    scan::{Crate, Hazard, macro_attribute},
    source::{Text, bare, coalesce, format_captures, is_assignment, names_in, string_literal},
};

mod calls;
mod fields;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    AcrossStatements,
    AcrossAwait,
    TwoLocks,
    ControlFlow,
    InsideActor,
    Calls,
    RwLockRead,
    Nested(&'static str),
    Poll,
    Unfollowed(&'static str),
}

impl Reason {
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::AcrossStatements => "a-guard-held-across-statements",
            Self::AcrossAwait => "a-lock-held-across-an-await",
            Self::RwLockRead => "rwlock",
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
            Self::RwLockRead => {
                "its read guard does more than copy the state, so fix can't prove that serializing its readers is safe"
            }
            Self::Nested(detail) | Self::Unfollowed(detail) => detail,
            Self::Poll => "its future polling context is unproved; the rewrite remains manual",
        }
    }
}

pub(super) struct Refusal {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) kind: LockKind,
    pub(super) reason: Reason,
    pub(super) boundary: Option<String>,
}

pub(super) struct Outcome {
    pub(super) edits: Vec<Edit>,
    pub(super) refusals: Vec<Refusal>,
}

pub(super) struct Draft {
    file: PathBuf,
    groups: Vec<Group>,
    links: Vec<(usize, usize)>,
    callees: Vec<Followed>,
}

/// A function whose parameters or return value `fix` rewrites, with the
/// number of calls it followed and the groups those rewrites belong to.
struct Followed {
    name: String,
    calls: usize,
    groups: Vec<usize>,
}

const ACTOR: &str = "::adamantium::Actor";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LockKind {
    Mutex,
    RwLock,
}

impl LockKind {
    pub(super) fn description(self) -> &'static str {
        match self {
            Self::Mutex => "Arc<Mutex<_>>",
            Self::RwLock => "Arc<RwLock<_>>",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum LockEvidence {
    Type(LockKind),
    Read,
    Write,
}

pub(super) type LockSpans = BTreeMap<(usize, usize), LockEvidence>;

pub(super) const IMPORTS: &[&[&str]] = &[
    &["std", "sync"],
    &["alloc", "sync"],
    &["std", "sync", "Arc"],
    &["alloc", "sync", "Arc"],
    &["std", "sync", "Mutex"],
    &["std", "sync", "RwLock"],
];

const UNLOCATED: Reason = Reason::Unfollowed("fix couldn't find its source text");

const ATTRIBUTED: Reason = Reason::Unfollowed("a statement holding its guard has attributes");

const NAME_TAKEN: Reason =
    Reason::Unfollowed("a constant, static, struct or import in the file has its name");

const IN_MACRO: Reason = Reason::Unfollowed("it is used in a macro");

const OUTSIDE_STATEMENT: Reason = Reason::Unfollowed("a guard is outside any statement");

const SHARED: Reason =
    Reason::Unfollowed("it holds the same state as a value fix leaves for review");

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

pub(super) fn plan(file: &Path, source: &str, locks: &LockSpans, krate: &Crate) -> Draft {
    let mut draft = Draft {
        file: file.to_path_buf(),
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
        locks,
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
            Followed {
                name: callee.name,
                calls: callee.calls,
                groups,
            }
        })
        .collect();
    draft
}

pub(super) fn settle(mut drafts: Vec<Draft>, krate: &Crate) -> Vec<(PathBuf, Outcome)> {
    for draft in &mut drafts {
        draft.share();
    }
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
        for callee in &self.callees {
            if krate.occurrences(&callee.name) != callee.calls + 1 {
                for &group in &callee.groups {
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
            let (reason, boundary) = if groups.iter().any(|group| group.escapes) {
                (
                    Some(Reason::Nested(
                        "it is locked in a closure or an async block, which might run inside an actor's callback",
                    )),
                    None,
                )
            } else {
                let locking = groups
                    .iter()
                    .flat_map(|g| g.functions.iter().cloned())
                    .collect();
                let blocks = groups
                    .iter()
                    .flat_map(|g| g.poll_blocks.iter().cloned())
                    .collect();
                let reached = krate.callers(&locking);
                match krate.poll_hazard(&reached, &blocks) {
                    Some(boundary) => (Some(Reason::Poll), Some(boundary)),
                    None => (Self::hazard(&reached, krate, callbacks), None),
                }
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

    fn hazard(
        reached: &BTreeSet<String>,
        krate: &Crate,
        callbacks: &BTreeSet<String>,
    ) -> Option<Reason> {
        if let Some(hazard) = krate.hazard(reached) {
            return Some(Reason::Nested(match hazard {
                Hazard::Trait => {
                    "it is locked through a trait method, which might run inside an actor's callback"
                }
                Hazard::Public => {
                    "it is locked through a public function, which another crate might call inside an actor's callback"
                }
                Hazard::Escapes => {
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback"
                }
                Hazard::Callbacks => {
                    "an actor callback in its package calls code that might lock it"
                }
            }));
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
                    kind: group.kind,
                    reason,
                    boundary: group.boundary,
                }),
                None => outcome.edits.extend(group.edits),
            }
        }
        outcome.edits = coalesce(outcome.edits);
        (self.file, outcome)
    }
}

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

#[derive(Default)]
struct Names {
    constants: BTreeSet<String>,
    macros: BTreeSet<String>,
    unsized_aliases: BTreeSet<String>,
    /// How many items and imports in the file declare each name.
    declared: HashMap<String, usize>,
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

pub(super) fn known_macro(name: &str) -> Option<usize> {
    match name {
        "format" | "print" | "println" | "eprint" | "eprintln" | "panic" => Some(0),
        "assert" | "debug_assert" | "write" | "writeln" => Some(1),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => Some(2),
        _ => None,
    }
}

struct MacroArgs {
    args: Vec<Expr>,
    format: usize,
    destination: bool,
    condition: bool,
    conditional: usize,
}

impl MacroArgs {
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

#[derive(Clone, PartialEq, Eq)]
enum Binding {
    Import(Vec<String>),
    Item,
    Parameter,
    Ambiguous,
}

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

enum Resolved {
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

const ARC_CRATES: &[&str] = &["std", "alloc"];
const ARC: &[&str] = &["sync", "Arc"];

/// The names of the first `segments` segments of `path`, when none of them has
/// generic arguments.
fn plain_names(path: &syn::Path, segments: usize) -> Option<Vec<String>> {
    path.segments
        .iter()
        .take(segments)
        .map(|segment| {
            matches!(segment.arguments, PathArguments::None).then(|| segment.ident.to_string())
        })
        .collect()
}

struct Var {
    name: String,
    spelling: String,
    group: Option<usize>,
    used: bool,
    field: bool,
    lock_ref: bool,
    record: Option<String>,
}

struct Group {
    name: String,
    line: usize,
    kind: LockKind,
    refusal: Option<Reason>,
    edits: Vec<Edit>,
    member: Option<String>,
    functions: BTreeSet<String>,
    poll_blocks: BTreeSet<String>,
    boundary: Option<String>,
    escapes: bool,
    callbacks: BTreeSet<String>,
}

impl Group {
    fn new(name: String, line: usize, kind: LockKind, edits: Vec<Edit>) -> Self {
        Self {
            name,
            line,
            kind,
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
    barrier: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Plain,
    Body,
}

#[derive(Clone, Copy)]
enum Node<'a> {
    Expr(&'a Expr),
    Stmt(&'a Stmt),
    Block(&'a Block, BlockKind),
    Macro(&'a MacroArgs),
}

#[derive(Clone, Copy)]
enum Root<'a> {
    Statement { full: Full<'a>, semi: bool },
    Let(&'a Local, &'a Expr),
    Value(&'a Expr),
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
            Self::Statement { full, .. } => full,
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
    locks: &'a LockSpans,
    names: &'a Names,
    macros: &'a HashMap<usize, MacroArgs>,
    vars: Vec<Var>,
    scopes: Vec<Scope>,
    items: Vec<ItemScope>,
    stack: Vec<Node<'a>>,
    groups: Vec<Group>,
    pending: Vec<(usize, usize)>,
    awaiting: Vec<(usize, &'a Local)>,
    fields: HashMap<String, fields::Field>,
    impls: Vec<Option<String>>,
    owners: Vec<Option<String>>,
    functions: Vec<(String, usize)>,
    links: Vec<(usize, usize)>,
    callees: Vec<calls::Callee>,
    callee_names: HashMap<String, Option<usize>>,
    signatures: HashMap<usize, usize>,
    returns: Vec<Option<calls::Frame<'a>>>,
}

impl<'a> Walker<'a> {
    fn edit(&self, range: Range<usize>, replacement: String) -> Edit {
        Edit::new(self.file, range, replacement)
    }

    fn refuse(&mut self, var: usize, reason: Reason) {
        if let Some(group) = self.vars.get(var).and_then(|var| var.group)
            && let Some(group) = self.groups.get_mut(group)
        {
            group.refuse(reason);
        }
    }

    fn mark_used(&mut self, var: usize) {
        if let Some(var) = self.vars.get_mut(var) {
            var.used = true;
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
            lock_ref: false,
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

    fn mention(&mut self, name: &str, reason: Reason) {
        if let Some(var) = self.lookup(name) {
            self.mark_used(var);
            self.refuse(var, reason);
        }
    }

    fn local(&self, expr: &Expr) -> Option<usize> {
        let Expr::Path(path) = expr else {
            return None;
        };
        if path.qself.is_some() || !path.attrs.is_empty() {
            return None;
        }
        self.lookup(&bare(path.path.get_ident()?))
    }

    fn tracked(&self, expr: &Expr) -> Option<usize> {
        self.field_place(expr)
            .or_else(|| self.local(expr).filter(|&var| self.group_of(var).is_some()))
    }

    fn holder(&self, var: usize, expr: &Expr) -> Option<String> {
        let var = self.vars.get(var)?;
        if var.field {
            Some(self.text.slice(expr)?.to_owned())
        } else {
            Some(var.spelling.clone())
        }
    }

    fn resolve_name(&self, name: &str) -> Resolved {
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

    fn resolve_path(&self, path: &syn::Path, segments: usize) -> Option<Vec<String>> {
        self.resolve_names(path.leading_colon.is_some(), plain_names(path, segments)?)
    }

    /// Resolves a path given as its segment names; an `absolute` one starts
    /// with `::`.
    fn resolve_names(&self, absolute: bool, names: Vec<String>) -> Option<Vec<String>> {
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

    fn is_arc(&self, path: &syn::Path, skip: usize) -> bool {
        let Some(count) = path.segments.len().checked_sub(skip) else {
            return false;
        };
        self.names_item(self.resolve_path(path, count), ARC_CRATES, ARC)
    }

    /// Whether `resolved` is the item `tail` of one of the crates `roots`.
    fn names_item(&self, resolved: Option<Vec<String>>, roots: &[&str], tail: &[&str]) -> bool {
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

    fn lock_kind(&self, range: Range<usize>) -> Option<LockKind> {
        match self.locks.get(&(range.start, range.end))? {
            LockEvidence::Type(kind) => Some(*kind),
            _ => None,
        }
    }

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

    fn actor_of(&self, state: &Type) -> Option<String> {
        Some(format!("{ACTOR}<{}>", self.text.slice(state)?))
    }

    fn arc_state<'t>(&self, ty: &'t Type) -> Option<(LockKind, &'t Type)> {
        let Type::Path(arc) = ty else {
            return None;
        };
        if arc.qself.is_some() {
            return None;
        }
        let last = arc.path.segments.last()?;
        let PathArguments::AngleBracketed(args) = &last.arguments else {
            return None;
        };
        let resolved = plain_names(&arc.path, arc.path.segments.len().saturating_sub(1)).and_then(
            |mut names| {
                names.push(last.ident.to_string());
                self.resolve_names(arc.path.leading_colon.is_some(), names)
            },
        );
        if args.args.len() != 1 || !self.names_item(resolved, ARC_CRATES, ARC) {
            return None;
        }
        let Some(GenericArgument::Type(lock)) = args.args.first() else {
            return None;
        };
        self.lock_state(lock)
    }

    fn lock_state<'t>(&self, ty: &'t Type) -> Option<(LockKind, &'t Type)> {
        let Type::Path(lock) = ty else {
            return None;
        };
        if lock.qself.is_some() {
            return None;
        }
        let kind = self.lock_kind(self.text.of(lock)?)?;
        let PathArguments::AngleBracketed(inner) = &lock.path.segments.last()?.arguments else {
            return None;
        };
        let Some(GenericArgument::Type(state)) = inner.args.first() else {
            return None;
        };
        if inner.args.len() != 1 || is_unsized(state, &self.names.unsized_aliases) {
            return None;
        }
        Some((kind, state))
    }

    fn constructor(&self, expr: &Expr) -> Option<(LockKind, String)> {
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
        let Some(Expr::Call(lock)) = arc.args.first() else {
            return None;
        };
        let Expr::Path(lock_new) = &*lock.func else {
            return None;
        };
        let last = lock_new.path.segments.last()?;
        if lock_new.qself.is_some()
            || lock_new
                .path
                .segments
                .iter()
                .any(|segment| !matches!(segment.arguments, PathArguments::None))
        {
            return None;
        }
        let kind = self.lock_kind(self.type_prefix(&lock_new.path)?)?;
        let replacement = match (last.ident.to_string().as_str(), lock.args.first()) {
            ("new", Some(state)) if lock.args.len() == 1 => {
                format!("{ACTOR}::new({})", self.text.slice(state)?)
            }
            ("default", None) => format!("{ACTOR}::default()"),
            _ => return None,
        };
        Some((kind, replacement))
    }

    fn clone_of(&self, expr: &Expr) -> Option<(usize, Option<String>)> {
        match expr {
            Expr::Path(_) => self.tracked(expr).map(|var| (var, None)),
            Expr::MethodCall(call)
                if call.method == "clone" && call.args.is_empty() && call.turbofish.is_none() =>
            {
                self.tracked(&call.receiver)
                    .filter(|&var| !self.vars.get(var).is_some_and(|var| var.lock_ref))
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
                Some(ty) => self.arc_state(ty).and_then(|(_, state)| {
                    Some(Some(self.edit(self.text.of(ty)?, self.actor_of(state)?)))
                }),
            };
            if let Some((kind, constructor)) = self.constructor(init)
                && let Some(ty_edit) = ty_edit.clone()
                && let Some(range) = self.text.of(init)
            {
                self.visit_expr(init);
                let edits = std::iter::once(self.edit(range, constructor))
                    .chain(ty_edit)
                    .collect();
                self.bind(local, name, spelling, kind, edits);
                return;
            }
            if let Some(ty_edit) = ty_edit.clone()
                && let Some(returned) = self.returned_by(init)
                && let Some(state) = self.groups.get(returned)
            {
                let var = self.bind(
                    local,
                    name,
                    spelling,
                    state.kind,
                    ty_edit.into_iter().collect(),
                );
                if let Some(group) = self.group_of(var) {
                    self.links.push((group, returned));
                }
                return;
            }
            if let Some((source, replacement)) = self.clone_of(init) {
                self.mark_used(source);
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
                    self.refuse(source, NAME_TAKEN);
                }
                let lock_ref = matches!(init, Expr::Path(_))
                    && self.vars.get(source).is_some_and(|var| var.lock_ref);
                let var = self.declare(name, group);
                if let Some(entry) = self.vars.get_mut(var) {
                    entry.spelling = spelling;
                    entry.lock_ref = lock_ref;
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

    fn bind(
        &mut self,
        local: &'a Local,
        name: String,
        spelling: String,
        kind: LockKind,
        edits: Vec<Edit>,
    ) -> usize {
        let group = self.groups.len();
        let mut state = Group::new(
            name.clone(),
            self.text.line(local.let_token.span),
            kind,
            edits,
        );
        if !local.attrs.is_empty() {
            state.refuse(Reason::Unfollowed("its declaration has attributes"));
        }
        if self.names.constants.contains(&name) {
            state.refuse(NAME_TAKEN);
        }
        self.groups.push(state);
        let var = self.declare(name, Some(group));
        if let Some(entry) = self.vars.get_mut(var) {
            entry.spelling = spelling;
        }
        self.settle(local, Some(var));
        var
    }

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

struct Plan<'a> {
    root: Root<'a>,
    narrow: Option<&'a Expr>,
    mode: Mode,
    receiver: bool,
}

fn is(expr: &Expr, child: &Expr) -> bool {
    std::ptr::eq(expr, child)
}

fn is_comparison(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq(_) | BinOp::Ne(_) | BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_)
    )
}

impl<'a> Walker<'a> {
    fn guard_access<'e>(&self, expr: &'e Expr) -> Option<(usize, &'e Expr, Option<Mode>)> {
        let Expr::MethodCall(unwrap) = expr else {
            return None;
        };
        let Expr::MethodCall(lock) = &*unwrap.receiver else {
            return None;
        };
        if !is_unwrap(unwrap) || !lock.args.is_empty() || lock.turbofish.is_some() {
            return None;
        }
        let var = self.tracked(&lock.receiver)?;
        let kind = self.groups.get(self.group_of(var)?)?.kind;
        let mode = match kind {
            LockKind::Mutex if lock.method == "lock" => None,
            LockKind::Mutex => return None,
            LockKind::RwLock => {
                let range = self.text.of(&lock.method)?;
                match self.locks.get(&(range.start, range.end))? {
                    LockEvidence::Read => Some(Mode::Read),
                    LockEvidence::Write => Some(Mode::With),
                    LockEvidence::Type(_) => return None,
                }
            }
        };
        Some((var, &lock.receiver, mode))
    }

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
            None => self.names_item(
                self.resolve_path(&func.path, func.path.segments.len()),
                &["std", "core"],
                &["mem", "drop"],
            ),
        };
        is_drop.then(|| self.tracked(call.args.first()?)).flatten()
    }

    fn root(&self) -> Result<Root<'a>, Reason> {
        let mut index = self.stack.len().checked_sub(1).ok_or(UNLOCATED)?;
        let Some(Node::Expr(mut child)) = self.stack.get(index).copied() else {
            return Err(OUTSIDE_STATEMENT);
        };
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
                    Stmt::Expr(_, Some(_)) => {
                        return Ok(Root::Statement {
                            full: Full::Expr(child),
                            semi: true,
                        });
                    }
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
                            return Ok(Root::Statement {
                                full: Full::Expr(child),
                                semi: false,
                            });
                        }
                        if kind == BlockKind::Body {
                            return Ok(Root::Value(child));
                        }
                        index = index.saturating_sub(1);
                        from_block = true;
                    }
                    Stmt::Macro(mac) => {
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
                        return Ok(Root::Statement {
                            full: Full::Macro(&mac.mac),
                            semi: mac.semi_token.is_some(),
                        });
                    }
                    Stmt::Item(_) => return Err(Reason::Unfollowed("a guard is in an item")),
                },
                Node::Block(..) | Node::Macro(_) => {}
            }
        }
        Err(OUTSIDE_STATEMENT)
    }

    fn plan(&self, guard: &'a Expr) -> Result<Plan<'a>, Reason> {
        let top = self.stack.len().checked_sub(1).ok_or(UNLOCATED)?;
        let mut chain = guard;
        let mut index = top;
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

    fn guard(&mut self, var: usize, guard: &'a Expr, held: &Expr, access: Option<Mode>) {
        self.mark_used(var);
        match self.guard_edit(var, guard, held, access) {
            Ok((edit, runs)) => {
                self.lock_site(var, runs);
                self.add_edit(var, edit);
            }
            Err(reason) => self.refuse(var, reason),
        }
    }

    fn guard_edit(
        &mut self,
        var: usize,
        guard: &'a Expr,
        held: &Expr,
        access: Option<Mode>,
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
        let holder = self.holder(var, held).ok_or(UNLOCATED)?;
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
        if access == Some(Mode::Read) && !copies_guard(full, guard) {
            return Err(Reason::RwLockRead);
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
        let state = if plan.receiver {
            format!("(*{name})")
        } else {
            name.clone()
        };
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
            let method = access.unwrap_or(Mode::Read).method();
            let replacement = format!("{holder}.{method}(|{name}| {})", substitute(range.clone())?);
            return Ok((self.edit(range, replacement), runs));
        }
        let method = access.unwrap_or(plan.mode).method();
        let (range, replacement) = match plan.root {
            Root::Statement { full, semi } => {
                let (range, assignment, attributed) = match full {
                    Full::Expr(expr) => (
                        self.text.of(expr),
                        matches!(expr, Expr::Assign(_))
                            || matches!(expr, Expr::Binary(binary) if is_assignment(&binary.op)),
                        self.text.attributed(expr),
                    ),
                    Full::Macro(mac) => (self.text.of(mac), false, false),
                };
                let range = range.ok_or(UNLOCATED)?;
                if attributed || self.statement_attributes() {
                    return Err(ATTRIBUTED);
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
                    return Err(ATTRIBUTED);
                }
                if let Some(group) = self.group_of(var) {
                    self.awaiting.push((group, local));
                }
                let range = self.text.of(expr).ok_or(UNLOCATED)?;
                let body = substitute(range.clone())?;
                (range, format!("{holder}.{method}(|{name}| {body})"))
            }
            Root::Value(expr) | Root::Condition(expr) => {
                if self.text.attributed(expr) {
                    return Err(ATTRIBUTED);
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

    /// Visits `expr` where it is the whole body of a closure or an `else`,
    /// so a plain block's last expression is the body's value.
    fn body(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Block(body) if body.attrs.is_empty() && body.label.is_none() => {
                self.stack.push(Node::Expr(expr));
                self.block(&body.block, BlockKind::Body);
                self.stack.pop();
            }
            body => self.visit_expr(body),
        }
    }

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

    fn mentions(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.mentions(group.stream()),
                TokenTree::Ident(ident) => {
                    let name = bare(&ident);
                    self.mention(&name, IN_MACRO);
                    self.mention_field(&name, IN_MACRO);
                }
                TokenTree::Literal(literal) => {
                    if let Some(string) = string_literal(&literal) {
                        for name in format_captures(&string) {
                            self.mention(&name, IN_MACRO);
                        }
                    }
                }
                TokenTree::Punct(_) => {}
            }
        }
    }
}

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

fn copies_guard(full: Full<'_>, guard: &Expr) -> bool {
    fn unparen(mut expr: &Expr) -> &Expr {
        while let Expr::Paren(paren) = expr {
            expr = &paren.expr;
        }
        expr
    }
    matches!(full, Full::Expr(expr) if matches!(unparen(expr), Expr::Unary(unary)
        if matches!(unary.op, UnOp::Deref(_)) && is(unparen(&unary.expr), guard)))
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
        if let Some((var, held, access)) = self.guard_access(expr) {
            self.stack.push(Node::Expr(expr));
            self.guard(var, expr, held, access);
            self.stack.pop();
            return;
        }
        if let Some((var, replacement)) = self.unwrapped(expr) {
            self.mark_used(var);
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
            self.mark_used(var);
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
            Expr::Closure(closure) => {
                self.returns.push(None);
                self.scoped(|walker| {
                    for input in &closure.inputs {
                        walker.declare_pattern(input);
                    }
                    walker.body(&closure.body);
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
                    self.body(other);
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

struct Scan<'w, 'a> {
    walker: &'w Walker<'a>,
    guard: &'a Expr,
    deferred: usize,
    branch: usize,
    awaits: bool,
    locks: bool,
    leaves: bool,
    calls: bool,
    unfollowed: Option<&'static str>,
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
            Expr::Call(call)
                if callee_name(&call.func).is_some_and(|name| LOCKS.contains(&name.as_str())) =>
            {
                self.locks = true;
            }
            Expr::Call(_) => self.calls = true,
            Expr::MethodCall(call) if !self.computed_from_state(&call.receiver) => {
                self.calls = true
            }
            Expr::Index(index) if !self.computed_from_state(&index.expr) => self.calls = true,
            Expr::Field(field) if !self.computed_from_state(&field.base) => self.calls = true,
            Expr::Unary(unary)
                if !matches!(&*unary.expr, Expr::Lit(_))
                    && !self.computed_from_state(&unary.expr) =>
            {
                self.calls = true;
            }
            Expr::Assign(assign) if !self.computed_from_state(&assign.left) => self.calls = true,
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

    fn visit_local(&mut self, local: &'a Local) {
        if self.deferred == 0 {
            self.calls = true;
        }
        syn::visit::visit_local(self, local);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::source::prune_imports;

    pub(super) fn mutex_spans(source: &str, mutexes: &[&str]) -> LockSpans {
        mutexes
            .iter()
            .flat_map(|mutex| {
                source.match_indices(mutex).map(|(start, _)| {
                    (
                        (start, start + mutex.len()),
                        LockEvidence::Type(LockKind::Mutex),
                    )
                })
            })
            .collect()
    }

    pub(super) fn outcome(source: &str, mutexes: &[&str]) -> Outcome {
        let spans = mutex_spans(source, mutexes);
        let file = Path::new("lib.rs");
        let mut krate = Crate::default();
        krate.add(file, source);
        let mut settled = settle(vec![plan(file, source, &spans, &krate)], &krate);
        settled.pop().map(|(_, outcome)| outcome).unwrap()
    }

    pub(super) fn fix(source: &str, mutexes: &[&str]) -> (String, Vec<(String, Reason)>) {
        let outcome = outcome(source, mutexes);
        let mut edits = outcome.edits;
        let imports = prune_imports(Path::new("lib.rs"), source, &edits, IMPORTS);
        edits.extend(imports);
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
        let edits = outcome(source, &["Mutex"]).edits;
        assert_eq!(
            prune_imports(Path::new("lib.rs"), source, &edits, IMPORTS).len(),
            1
        );
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
            "fn f(other: &str) { let m = Arc::new(Mutex::new(0u32)); let _s = format!(\"{} {}\", *m.lock().unwrap(), other); }",
            "fn f(other: &str) { let m = Arc::new(Mutex::new(0u32)); let _s = format!(\"{} {other}\", *m.lock().unwrap()); }",
            "const OTHER: &str = \"\";\nfn f() { let m = Arc::new(Mutex::new(0u32)); assert_eq!(*m.lock().unwrap(), 0, \"{}\", OTHER); }",
            "fn f(other: &str) { let m = Arc::new(Mutex::new(\"\")); assert_eq!(*m.lock().unwrap(), other); }",
            "fn f(out: &mut String) { use std::fmt::Write; let m = Arc::new(Mutex::new(0u32)); writeln!(out, \"{}\", *m.lock().unwrap()).unwrap(); }",
        ] {
            refused(body, "m", Reason::Calls);
        }
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
}
