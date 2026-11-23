use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};

use proc_macro2::{TokenStream, TokenTree};
use syn::{
    Attribute, Block, Expr, ExprMethodCall, Item, Lit, Local, Macro, Pat, PathArguments, Stmt,
    visit::Visit,
};

use super::{
    Edit,
    scan::macro_attribute,
    source::{Text, bare, format_captures, is_assignment, string_literal},
    state::pattern_names,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Oneshot,
    Watch,
    Mpsc,
}

impl Kind {
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::Oneshot => "a-single-reply",
            Self::Watch => "the-latest-value",
            Self::Mpsc => "a-bounded-queue",
        }
    }

    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Oneshot => "a Tokio oneshot channel",
            Self::Watch => "a Tokio watch channel",
            Self::Mpsc => "a Tokio mpsc channel",
        }
    }
}

/// Where each flagged constructor call starts, with where it ends and its kind.
pub(super) type Constructors = BTreeMap<usize, (usize, Kind)>;

pub(super) struct Refusal {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) kind: Kind,
    pub(super) reason: &'static str,
}

pub(super) struct Outcome {
    pub(super) edits: Vec<Edit>,
    pub(super) refusals: Vec<Refusal>,
}

const REPLY: &str = "::adamantium::Reply";
const WATCH: &str = "::adamantium::Watch";
const CHANNEL: &str = "::adamantium::Channel";

pub(super) const IMPORTS: &[&[&str]] = &[
    &["tokio", "sync"],
    &["tokio", "sync", "oneshot"],
    &["tokio", "sync", "oneshot", "channel"],
    &["tokio", "sync", "watch"],
    &["tokio", "sync", "watch", "channel"],
    &["tokio", "sync", "mpsc"],
    &["tokio", "sync", "mpsc", "channel"],
];

const UNBOUND: &str =
    "its sender and receiver aren't bound by a `let (sender, receiver) = ...` statement";
const TYPED: &str = "its halves have a type annotation that fix would have to rewrite";
const ATTRIBUTE: &str = "it is used under an attribute such as `#[cfg]`";
const ATTRIBUTE_MACRO: &str = "its function has an attribute macro, which might rewrite its body";
const MACRO: &str = "it is used in a macro";
const USE: &str = "a half is used where fix can't follow it, such as an argument, a field, a return value or a reference";
const RESULT: &str = "the result of a send or receive is used in a way whose meaning would change";
const UNLOCATED: &str = "fix couldn't find its source text";
const OVERLAP: &str = "rewriting it would overlap the rewrite of another channel";
const WATCH_SEND: &str = "a send's result is used, but Tokio's `send` fails once every receiver is gone and `Watch::send` keeps the value";
const WATCH_UNWATCHED: &str =
    "a send might run once no receiver is alive, when Tokio drops the value and `Watch` keeps it";
const WATCH_SENDER: &str = "its sender is read, subscribed to or asked about receivers, which would show a value Tokio dropped because no receiver was alive";
const HAS_CHANGED: &str = "Tokio's `has_changed` fails once the sender is gone, even with an unseen value, where `Watcher::has_changed` doesn't";
const BORROW_SEEN: &str = "`borrow` leaves the value unseen, but `Watcher::get` marks it seen, and the receiver also waits for changes";
const AWAIT_CAPTURED: &str = "it is awaited inside a closure or an async block without `move`, which would borrow the receiver that awaiting Tokio's moves";
const CAPACITY: &str = "its capacity isn't a positive integer literal, and Tokio panics on zero where `Channel` rounds it up to one";
const SENDER_CLONED: &str = "its sender is cloned, and Tokio admits waiting sends in the order they arrived, where `Channel` lets a later send take freed room first";
const SEND_SHARED: &str = "a send isn't awaited where it is called, or runs in a closure or an async block that borrows the sender, so two sends might wait at once, and `Channel` doesn't admit them in order as Tokio does";
const RECEIVE_UNAWAITED: &str =
    "a receive isn't awaited where it is called, so fix can't follow its future";
const CLOSE_VALUE: &str =
    "the value of `close` is used, which is `()` in Tokio and a `bool` for `Channel`";
const MPSC_METHOD: &str = "a half calls a method that `Channel` lacks or that means something else there, such as `reserve`, `recv_many`, `blocking_send`, `closed` or `capacity`";
const READ_IN_PLACE: &str = "`*rx.borrow()` reads the value where it is, as in a comparison, without the copy that `Watcher::get` would make with `Clone`";
const BORROW_CAPTURED: &str = "`borrow` would become `Watcher::get`, which needs the receiver mutably, inside a closure or async block";

pub(super) fn plan(file: &Path, source: &str, constructors: &Constructors) -> Outcome {
    let mut outcome = Outcome {
        edits: Vec::new(),
        refusals: Vec::new(),
    };
    let Ok(tree) = syn::parse_file(source) else {
        return outcome;
    };
    let text = Text::new(source, &tree);
    let mut finder = Finder {
        text: &text,
        constructors,
        hazards: 0,
        found: Vec::new(),
    };
    finder.visit_file(&tree);
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for found in finder.found {
        seen.insert(found.start);
        groups.push(group(file, &text, &found));
    }
    for (&start, &(end, kind)) in constructors {
        if !seen.contains(&start) {
            let line = source
                .get(..start)
                .map_or(1, |before| before.matches('\n').count() + 1);
            groups.push(Group {
                line,
                name: source
                    .get(start..end)
                    .map_or_else(|| "channel".to_owned(), |callee| format!("{callee}(..)")),
                kind,
                edits: Vec::new(),
                refusal: Some(UNBOUND),
            });
        }
    }
    refuse_overlaps(&mut groups);
    for group in groups {
        match group.refusal {
            Some(reason) => outcome.refusals.push(Refusal {
                line: group.line,
                name: group.name,
                kind: group.kind,
                reason,
            }),
            None => outcome.edits.extend(group.edits),
        }
    }
    outcome.refusals.sort_by_key(|refusal| refusal.line);
    outcome
}

struct Group {
    line: usize,
    name: String,
    kind: Kind,
    edits: Vec<Edit>,
    refusal: Option<&'static str>,
}

fn refuse_overlaps(groups: &mut [Group]) {
    let mut overlapping = BTreeSet::new();
    for (index, group) in groups.iter().enumerate() {
        for (other_index, other) in groups.iter().enumerate().skip(index + 1) {
            if group.refusal.is_some() || other.refusal.is_some() {
                continue;
            }
            let touches = group.edits.iter().any(|edit| {
                other
                    .edits
                    .iter()
                    .any(|other| edit.start <= other.end && other.start <= edit.end)
            });
            if touches {
                overlapping.insert(index);
                overlapping.insert(other_index);
            }
        }
    }
    for index in overlapping {
        if let Some(group) = groups.get_mut(index) {
            group.refusal = Some(OVERLAP);
        }
    }
}

struct Found<'a> {
    start: usize,
    kind: Kind,
    local: &'a Local,
    rest: &'a [Stmt],
    refusal: Option<&'static str>,
}

struct Finder<'t, 'c> {
    text: &'t Text<'t>,
    constructors: &'c Constructors,
    hazards: usize,
    found: Vec<Found<'t>>,
}

impl<'t> Finder<'t, '_> {
    fn constructor(&self, expr: &Expr) -> Option<(usize, Kind)> {
        let Expr::Call(call) = expr else {
            return None;
        };
        let range = self.text.of(&call.func)?;
        let &(end, kind) = self.constructors.get(&range.start)?;
        (end == range.end).then_some((range.start, kind))
    }

    /// Visits an item, counting it as a hazard when an attribute macro on it
    /// might rewrite what it contains.
    fn within(&mut self, attrs: &[Attribute], visit: impl FnOnce(&mut Self)) {
        let hazard = usize::from(
            attrs
                .iter()
                .any(|attr| macro_attribute(std::slice::from_ref(attr)) && !runtime_entry(attr)),
        );
        self.hazards += hazard;
        visit(self);
        self.hazards -= hazard;
    }
}

fn runtime_entry(attr: &Attribute) -> bool {
    let segments: Vec<String> = attr
        .path()
        .segments
        .iter()
        .map(|segment| bare(&segment.ident))
        .collect();
    segments == ["tokio", "main"] || segments == ["tokio", "test"]
}

impl<'t> Visit<'t> for Finder<'t, '_> {
    fn visit_item_fn(&mut self, item: &'t syn::ItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_fn(finder, item)
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'t syn::ImplItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_impl_item_fn(finder, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'t syn::TraitItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_trait_item_fn(finder, item)
        });
    }

    fn visit_item_impl(&mut self, item: &'t syn::ItemImpl) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_impl(finder, item)
        });
    }

    fn visit_item_trait(&mut self, item: &'t syn::ItemTrait) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_trait(finder, item)
        });
    }

    fn visit_item_mod(&mut self, item: &'t syn::ItemMod) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_mod(finder, item)
        });
    }

    fn visit_block(&mut self, block: &'t Block) {
        for (index, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Local(local) = stmt else {
                continue;
            };
            let Some((start, kind)) = local
                .init
                .as_ref()
                .and_then(|init| self.constructor(&init.expr))
            else {
                continue;
            };
            let refusal = if self.hazards > 0 {
                Some(ATTRIBUTE_MACRO)
            } else if !local.attrs.is_empty() {
                Some(ATTRIBUTE)
            } else {
                None
            };
            self.found.push(Found {
                start,
                kind,
                local,
                rest: block.stmts.get(index + 1..).unwrap_or_default(),
                refusal,
            });
        }
        syn::visit::visit_block(self, block);
    }
}

fn halves(pat: &Pat) -> Result<[&syn::PatIdent; 2], &'static str> {
    let Pat::Tuple(tuple) = pat else {
        return Err(if matches!(pat, Pat::Type(_)) {
            TYPED
        } else {
            UNBOUND
        });
    };
    let names: Vec<&syn::PatIdent> = tuple
        .elems
        .iter()
        .filter_map(|pat| match pat {
            Pat::Ident(ident)
                if ident.by_ref.is_none() && ident.subpat.is_none() && ident.attrs.is_empty() =>
            {
                Some(ident)
            }
            _ => None,
        })
        .collect();
    match names.as_slice() {
        [sender, receiver] if tuple.elems.len() == 2 => Ok([*sender, *receiver]),
        _ => Err(UNBOUND),
    }
}

fn group(file: &Path, text: &Text<'_>, found: &Found<'_>) -> Group {
    let pat_text = text.slice(&found.local.pat).unwrap_or("channel");
    let mut group = Group {
        line: text.line(found.local.let_token.span),
        name: pat_text.to_owned(),
        kind: found.kind,
        edits: Vec::new(),
        refusal: found.refusal,
    };
    if group.refusal.is_some() {
        return group;
    }
    let call = match found.local.init.as_ref() {
        Some(init) if init.diverge.is_none() => match &*init.expr {
            Expr::Call(call) => call,
            _ => {
                group.refusal = Some(UNBOUND);
                return group;
            }
        },
        _ => {
            group.refusal = Some(UNBOUND);
            return group;
        }
    };
    let halves = match halves(&found.local.pat) {
        Ok(halves) => halves,
        Err(reason) => {
            group.refusal = Some(reason);
            return group;
        }
    };
    let arguments = match found.kind {
        Kind::Oneshot => 0,
        Kind::Watch | Kind::Mpsc => 1,
    };
    let Some(turbofish) = turbofish(text, &call.func) else {
        group.refusal = Some(UNBOUND);
        return group;
    };
    if call.args.len() != arguments {
        group.refusal = Some(UNBOUND);
        return group;
    }
    if found.kind == Kind::Mpsc && !call.args.first().is_some_and(positive_capacity) {
        group.refusal = Some(CAPACITY);
        return group;
    }
    let mut analysis = Analysis {
        text,
        file,
        kind: found.kind,
        vars: Vec::new(),
        scopes: Vec::new(),
        edits: Vec::new(),
        refusal: None,
        captured: 0,
        nested: 0,
        sends: false,
        sends_nested: false,
        receiver_moved: false,
        waits: false,
        borrows: false,
    };
    let mut scope = Vec::new();
    for (role, half) in [Role::Sender, Role::Receiver].into_iter().zip(halves) {
        let var = analysis.var(role, half);
        scope.push((bare(&half.ident), Some(var)));
    }
    analysis.scopes.push(scope);
    for stmt in found.rest {
        analysis.visit_stmt(stmt);
    }
    if analysis.borrows && analysis.waits {
        analysis.refuse(BORROW_SEEN);
    }
    if found.kind == Kind::Watch
        && analysis.sends
        && (analysis.sends_nested || analysis.receiver_moved)
    {
        analysis.refuse(WATCH_UNWATCHED);
    }
    if let Some(reason) = analysis.refusal {
        group.refusal = Some(reason);
        return group;
    }
    let constructor = match found.kind {
        Kind::Oneshot => text
            .of(&call.func)
            .map(|range| vec![analysis.edit(range, format!("{REPLY}{turbofish}::pair"))]),
        Kind::Watch => watch_statement(text, found.local, &analysis, &turbofish, halves)
            .map(|(range, replacement)| vec![analysis.edit(range, replacement)]),
        Kind::Mpsc => text
            .of(call)
            .zip(call.args.first().and_then(|capacity| text.slice(capacity)))
            .map(|(range, capacity)| {
                vec![analysis.edit(
                    range,
                    format!("{CHANNEL}{turbofish}::bounded({capacity}).split()"),
                )]
            }),
    };
    let Some(constructor) = constructor else {
        group.refusal = Some(UNLOCATED);
        return group;
    };
    let mut edits = constructor;
    edits.append(&mut analysis.edits);
    for (index, var) in analysis.vars.iter().enumerate() {
        if found.kind == Kind::Watch && index < 2 {
            continue;
        }
        match (found.kind, var.role, &var.mutable) {
            (Kind::Oneshot | Kind::Mpsc, _, Some(mutable)) => {
                edits.push(analysis.edit(mutable.clone(), String::new()));
            }
            (Kind::Watch, Role::Receiver, None) if var.needs_mut => {
                edits.push(analysis.edit(var.ident.start..var.ident.start, "mut ".to_owned()));
            }
            _ => {}
        }
    }
    group.edits = edits;
    group
}

fn positive_capacity(capacity: &Expr) -> bool {
    let Expr::Lit(literal) = capacity else {
        return false;
    };
    let Lit::Int(int) = &literal.lit else {
        return false;
    };
    literal.attrs.is_empty()
        && matches!(int.suffix(), "" | "usize")
        && int
            .base10_parse::<u32>()
            .is_ok_and(|capacity| (1..=MAX_CAPACITY).contains(&capacity))
}

const MAX_CAPACITY: u32 = u32::MAX >> 3;

fn turbofish(text: &Text<'_>, func: &Expr) -> Option<String> {
    let Expr::Path(path) = func else {
        return None;
    };
    if path.qself.is_some() || !path.attrs.is_empty() {
        return None;
    }
    let (last, init) = path
        .path
        .segments
        .iter()
        .collect::<Vec<_>>()
        .split_last()
        .map(|(last, init)| {
            (
                *last,
                init.iter().all(|segment| segment.arguments.is_none()),
            )
        })?;
    if !init {
        return None;
    }
    match &last.arguments {
        PathArguments::None => Some(String::new()),
        PathArguments::AngleBracketed(arguments) if arguments.colon2_token.is_some() => {
            let range = text.of(arguments)?;
            text.get(range).map(str::to_owned)
        }
        _ => None,
    }
}

fn watch_statement(
    text: &Text<'_>,
    local: &Local,
    analysis: &Analysis<'_>,
    turbofish: &str,
    halves: [&syn::PatIdent; 2],
) -> Option<(Range<usize>, String)> {
    let range = text.of(local)?;
    let Some(Expr::Call(call)) = local.init.as_ref().map(|init| &*init.expr) else {
        return None;
    };
    let value = text.slice(call.args.first()?)?;
    let source = text.source;
    let line_start = source.get(..range.start)?.rfind('\n').map_or(0, |n| n + 1);
    let indent = source.get(line_start..range.start)?;
    let separator = if !indent.trim().is_empty() {
        " ".to_owned()
    } else if source.get(..line_start)?.ends_with("\r\n") {
        format!("\r\n{indent}")
    } else {
        format!("\n{indent}")
    };
    let [sender, receiver] = halves;
    let name = |ident: &syn::PatIdent| text.slice(&ident.ident);
    let sender_name = name(sender)?;
    let receiver_name = name(receiver)?;
    let sender_mut = if sender.mutability.is_some() {
        "mut "
    } else {
        ""
    };
    let receiver_mut =
        if receiver.mutability.is_some() || analysis.vars.get(1).is_some_and(|var| var.needs_mut) {
            "mut "
        } else {
            ""
        };
    Some((
        range,
        format!(
            "let {sender_mut}{sender_name} = {WATCH}{turbofish}::new({value});{separator}let {receiver_mut}{receiver_name} = {sender_name}.subscribe();"
        ),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Sender,
    Receiver,
}

struct Var {
    role: Role,
    ident: Range<usize>,
    mutable: Option<Range<usize>>,
    needs_mut: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Result,
    Sent,
    Option,
    Discard,
}

impl Shape {
    /// Why a result of this shape can't be used where fix doesn't follow it.
    fn misuse(self) -> &'static str {
        if self == Self::Discard {
            WATCH_SEND
        } else {
            RESULT
        }
    }
}

struct Op<'a> {
    var: usize,
    shape: Shape,
    kind: OpKind<'a>,
}

enum OpKind<'a> {
    Send(&'a Expr),
    AwaitedSend(&'a Expr),
    Await(&'a Expr),
    TryRecv(&'a syn::Ident),
    Changed,
}

/// A receiver's `borrow` or `borrow_and_update` call; only the second marks
/// the value seen.
struct Borrow<'e> {
    call: &'e ExprMethodCall,
    var: usize,
    marks_seen: bool,
}

struct Analysis<'t> {
    text: &'t Text<'t>,
    file: &'t Path,
    kind: Kind,
    vars: Vec<Var>,
    scopes: Vec<Vec<(String, Option<usize>)>>,
    edits: Vec<Edit>,
    refusal: Option<&'static str>,
    captured: usize,
    nested: usize,
    sends: bool,
    sends_nested: bool,
    receiver_moved: bool,
    waits: bool,
    borrows: bool,
}

impl<'t> Analysis<'t> {
    fn edit(&self, range: Range<usize>, replacement: String) -> Edit {
        Edit::new(self.file, range, replacement)
    }

    fn replace<T: syn::spanned::Spanned + ?Sized>(&mut self, node: &T, replacement: &str) {
        match self.text.of(node) {
            Some(range) => {
                let edit = self.edit(range, replacement.to_owned());
                self.edits.push(edit);
            }
            None => self.refuse(UNLOCATED),
        }
    }

    fn refuse(&mut self, reason: &'static str) {
        self.refusal.get_or_insert(reason);
    }

    fn var(&mut self, role: Role, ident: &syn::PatIdent) -> usize {
        let range = self.text.of(&ident.ident).unwrap_or_else(|| {
            self.refuse(UNLOCATED);
            0..0
        });
        let mutable = ident
            .mutability
            .as_ref()
            .and_then(|token| self.text.range(token.span))
            .map(|token| token.start..range.start);
        self.vars.push(Var {
            role,
            ident: range,
            mutable,
            needs_mut: false,
        });
        self.vars.len() - 1
    }

    fn tracked(&self, expr: &Expr) -> Option<usize> {
        let Expr::Path(path) = expr else {
            return None;
        };
        if path.qself.is_some()
            || !path.attrs.is_empty()
            || path.path.leading_colon.is_some()
            || path.path.segments.len() != 1
        {
            return None;
        }
        let segment = path.path.segments.first()?;
        if !segment.arguments.is_none() {
            return None;
        }
        self.lookup(&bare(&segment.ident))
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|(bound, _)| bound == name)
            .and_then(|(_, var)| *var)
    }

    fn role(&self, var: usize) -> Option<Role> {
        self.vars.get(var).map(|var| var.role)
    }

    fn method(&self, expr: &Expr, name: &str, role: Role) -> Option<usize> {
        let Expr::MethodCall(call) = expr else {
            return None;
        };
        let var = self.tracked(&call.receiver)?;
        (call.method == name
            && call.args.is_empty()
            && call.turbofish.is_none()
            && self.role(var) == Some(role))
        .then_some(var)
    }

    fn declare(&mut self, pat: &Pat) {
        let mut names = Vec::new();
        pattern_names(pat, &mut names);
        if let Some(scope) = self.scopes.last_mut() {
            scope.extend(names.into_iter().map(|name| (name, None)));
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.scopes.push(Vec::new());
        f(self);
        self.scopes.pop();
    }

    fn sent<'e>(&self, expr: &'e Expr, name: &str) -> Option<(usize, &'e Expr)> {
        let Expr::MethodCall(call) = expr else {
            return None;
        };
        if call.turbofish.is_some() || !call.attrs.is_empty() || call.method != name {
            return None;
        }
        let var = self.tracked(&call.receiver)?;
        let value = call.args.first()?;
        (call.args.len() == 1 && self.role(var) == Some(Role::Sender)).then_some((var, value))
    }

    fn op<'e>(&self, expr: &'e Expr) -> Option<Op<'e>> {
        match expr {
            Expr::MethodCall(call) if call.attrs.is_empty() => {
                let (send, sent) = match self.kind {
                    Kind::Oneshot => ("send", Shape::Sent),
                    Kind::Watch => ("send", Shape::Discard),
                    Kind::Mpsc => ("try_send", Shape::Sent),
                };
                if let Some((var, value)) = self.sent(expr, send) {
                    return Some(Op {
                        var,
                        shape: sent,
                        kind: OpKind::Send(value),
                    });
                }
                let received = match self.kind {
                    Kind::Oneshot => Shape::Result,
                    Kind::Mpsc => Shape::Sent,
                    Kind::Watch => return None,
                };
                let var = self.method(expr, "try_recv", Role::Receiver)?;
                Some(Op {
                    var,
                    shape: received,
                    kind: OpKind::TryRecv(&call.method),
                })
            }
            Expr::Await(awaited) if awaited.attrs.is_empty() => match (self.kind, &*awaited.base) {
                (Kind::Oneshot, base) => {
                    let var = self.tracked(base)?;
                    (self.role(var) == Some(Role::Receiver)).then_some(Op {
                        var,
                        shape: Shape::Option,
                        kind: OpKind::Await(base),
                    })
                }
                (Kind::Watch, Expr::MethodCall(call)) if call.attrs.is_empty() => {
                    let var = self.method(&awaited.base, "changed", Role::Receiver)?;
                    Some(Op {
                        var,
                        shape: Shape::Option,
                        kind: OpKind::Changed,
                    })
                }
                (Kind::Mpsc, base) => {
                    let (var, value) = self.sent(base, "send")?;
                    Some(Op {
                        var,
                        shape: Shape::Sent,
                        kind: OpKind::AwaitedSend(value),
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn take(&mut self, op: &Op<'t>) {
        match op.kind {
            OpKind::Send(value) => {
                if self.kind == Kind::Watch {
                    self.sends = true;
                    self.sends_nested |= self.nested > 0;
                }
                self.visit_expr(value);
            }
            OpKind::AwaitedSend(_) if self.captured > 0 => self.refuse(SEND_SHARED),
            OpKind::AwaitedSend(value) => self.visit_expr(value),
            OpKind::Await(_) if self.captured > 0 => self.refuse(AWAIT_CAPTURED),
            OpKind::Await(path) => match self.text.of(path) {
                Some(range) => {
                    let edit = self.edit(range.end..range.end, ".receive()".to_owned());
                    self.edits.push(edit);
                }
                None => self.refuse(UNLOCATED),
            },
            OpKind::TryRecv(method) => self.replace(method, "try_receive"),
            OpKind::Changed => {
                self.waits = true;
                if let Some(var) = self.vars.get_mut(op.var) {
                    var.needs_mut = true;
                }
            }
        }
    }

    fn statement(&mut self, expr: &'t Expr) -> bool {
        if let Some(op) = self.op(expr) {
            self.take(&op);
            return true;
        }
        match expr {
            Expr::MethodCall(call) if call.attrs.is_empty() && call.turbofish.is_none() => {
                if self.kind != Kind::Watch && self.method(expr, "close", Role::Receiver).is_some()
                {
                    return true;
                }
                let Some(op) = self.op(&call.receiver) else {
                    return false;
                };
                let method = call.method.to_string();
                let discarded = op.shape == Shape::Discard
                    && matches!(
                        (method.as_str(), call.args.len()),
                        ("ok" | "is_ok" | "is_err" | "unwrap_or_default", 0) | ("unwrap_or", 1)
                    );
                if !discarded {
                    return false;
                }
                self.take(&op);
                for arg in &call.args {
                    self.visit_expr(arg);
                }
                true
            }
            _ => false,
        }
    }

    fn ok_or_else(&mut self, call: &ExprMethodCall) {
        self.replace(&call.method, "ok_or_else");
        self.closure_without_argument(call);
    }

    fn closure_without_argument(&mut self, call: &ExprMethodCall) {
        if let Some(Expr::Closure(closure)) = call.args.first() {
            match (
                self.text.range(closure.inputs_begin.span),
                self.text.range(closure.inputs_end.span),
            ) {
                (Some(start), Some(end)) => {
                    let edit = self.edit(start.start..end.end, "||".to_owned());
                    self.edits.push(edit);
                }
                _ => self.refuse(UNLOCATED),
            }
        }
    }

    fn consumer(&mut self, call: &'t ExprMethodCall) -> bool {
        let Some(op) = self.op(&call.receiver) else {
            return false;
        };
        if call.turbofish.is_some() || !call.attrs.is_empty() {
            return false;
        }
        let method = call.method.to_string();
        let arguments = call.args.len();
        match (op.shape, method.as_str(), arguments) {
            (Shape::Result | Shape::Sent, "is_ok" | "is_err" | "ok" | "unwrap_or_default", 0)
            | (Shape::Result | Shape::Sent | Shape::Option, "unwrap_or", 1)
            | (Shape::Option, "unwrap_or_default", 0)
            | (Shape::Result, "unwrap", 0)
            | (Shape::Result, "expect", 1) => {
                self.take(&op);
                for arg in &call.args {
                    self.visit_expr(arg);
                }
            }
            (Shape::Option, "is_ok", 0) => {
                self.take(&op);
                self.replace(&call.method, "is_some");
            }
            (Shape::Option, "is_err", 0) => {
                self.take(&op);
                self.replace(&call.method, "is_none");
            }
            (Shape::Option, "ok", 0) => {
                self.take(&op);
                match (self.text.of(&call.receiver), self.text.of(call)) {
                    (Some(receiver), Some(whole)) => {
                        let edit = self.edit(receiver.end..whole.end, String::new());
                        self.edits.push(edit);
                    }
                    _ => self.refuse(UNLOCATED),
                }
            }
            (Shape::Result | Shape::Sent | Shape::Option, "map_err", 1)
            | (Shape::Option, "unwrap_or_else", 1) => {
                let Some(handler) = ignored_error(call) else {
                    return false;
                };
                self.take(&op);
                if op.shape == Shape::Option {
                    if method == "map_err" {
                        self.ok_or_else(call);
                    } else {
                        self.closure_without_argument(call);
                    }
                }
                self.visit_expr(handler);
            }
            _ => return false,
        }
        true
    }

    fn patterns(&mut self, op: &Op<'t>, pats: &[&Pat]) -> bool {
        let mut edits = Vec::new();
        for pat in pats {
            match self.pattern(op.shape, pat) {
                Some(mut pattern) => edits.append(&mut pattern),
                None => return false,
            }
        }
        self.take(op);
        self.edits.append(&mut edits);
        true
    }

    fn pattern(&self, shape: Shape, pat: &Pat) -> Option<Vec<Edit>> {
        if shape == Shape::Discard {
            return None;
        }
        if let Pat::Guard(guarded) = pat {
            return guarded
                .attrs
                .is_empty()
                .then(|| self.pattern(shape, &guarded.pat))
                .flatten();
        }
        let Pat::TupleStruct(variant) = pat else {
            return matches!(pat, Pat::Wild(wild) if wild.attrs.is_empty()).then(Vec::new);
        };
        if variant.qself.is_some() || !variant.attrs.is_empty() || variant.elems.len() != 1 {
            return None;
        }
        let name = variant.path.get_ident()?;
        let inner = variant.elems.first()?;
        let wild = matches!(inner, Pat::Wild(wild) if wild.attrs.is_empty());
        let whole = self.text.of(pat)?;
        let path = self.text.of(name)?;
        match (shape, name.to_string().as_str()) {
            (Shape::Result | Shape::Sent, "Ok") => Some(Vec::new()),
            (Shape::Option, "Ok") => Some(vec![self.edit(path, "Some".to_owned())]),
            (Shape::Result | Shape::Sent, "Err") if wild => Some(Vec::new()),
            (Shape::Option, "Err") if wild => Some(vec![self.edit(whole, "None".to_owned())]),
            _ => None,
        }
    }

    /// Visits `expr`, adapting `pat` when it takes apart the result of a send
    /// or receive.
    fn destructured(&mut self, expr: &'t Expr, pat: &Pat) {
        match self.op(expr) {
            Some(op) => {
                if !self.patterns(&op, &[pat]) {
                    self.refuse(RESULT);
                }
            }
            None => self.visit_expr(expr),
        }
    }

    fn mentions(&self, tokens: TokenStream) -> bool {
        tokens.into_iter().any(|token| match token {
            TokenTree::Group(group) => self.mentions(group.stream()),
            TokenTree::Ident(ident) => self.lookup(&bare(&ident)).is_some(),
            TokenTree::Literal(literal) => string_literal(&literal).is_some_and(|string| {
                format_captures(&string)
                    .iter()
                    .any(|name| self.lookup(name).is_some())
            }),
            TokenTree::Punct(_) => false,
        })
    }

    fn local(&mut self, local: &'t Local) {
        if local.attrs.iter().any(|attr| !lint_attribute(attr))
            && self.mentions(quote::ToTokens::to_token_stream(local))
        {
            self.refuse(ATTRIBUTE);
        }
        let Some(init) = &local.init else {
            self.declare(&local.pat);
            return;
        };
        if let Pat::Ident(binding) = &local.pat
            && binding.by_ref.is_none()
            && binding.subpat.is_none()
            && binding.attrs.is_empty()
            && init.diverge.is_none()
        {
            let moved = self.tracked(&init.expr).and_then(|var| self.role(var));
            let cloned = match &*init.expr {
                Expr::MethodCall(call) if self.kind == Kind::Watch && call.attrs.is_empty() => self
                    .method(&init.expr, "clone", Role::Sender)
                    .or_else(|| self.method(&init.expr, "clone", Role::Receiver))
                    .and_then(|var| self.role(var)),
                _ => None,
            };
            if self.tracked(&init.expr) == Some(RECEIVER) {
                self.receiver_moved = true;
            }
            if let Some(role) = moved.or(cloned) {
                let var = self.var(role, binding);
                if let Some(scope) = self.scopes.last_mut() {
                    scope.push((bare(&binding.ident), Some(var)));
                }
                return;
            }
        }
        match (&local.pat, &init.diverge) {
            (Pat::Wild(wild), None) if wild.attrs.is_empty() => {
                if !self.statement(&init.expr) {
                    self.visit_expr(&init.expr);
                }
            }
            (pat, Some((_, diverge))) => {
                self.destructured(&init.expr, pat);
                self.visit_expr(diverge);
            }
            (pat, None) if binds_whole(pat) => self.value(&init.expr),
            _ => self.visit_expr(&init.expr),
        }
        self.declare(&local.pat);
    }
}

fn is_drop(call: &syn::ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    let segments: Vec<String> = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    call.args.len() == 1
        && path.qself.is_none()
        && path
            .path
            .segments
            .iter()
            .all(|segment| segment.arguments.is_none())
        && (segments == ["drop"]
            || (path.path.segments.len() == 3
                && matches!(segments.first().map(String::as_str), Some("std" | "core"))
                && segments.get(1).is_some_and(|module| module == "mem")
                && segments.get(2).is_some_and(|name| name == "drop")))
}

fn ignored_error(call: &ExprMethodCall) -> Option<&Expr> {
    let mut args = call.args.iter();
    let (Some(argument @ Expr::Closure(closure)), None) = (args.next(), args.next()) else {
        return None;
    };
    let [Pat::Wild(wild)] = closure.inputs.iter().collect::<Vec<_>>().as_slice() else {
        return None;
    };
    (wild.attrs.is_empty()
        && closure.attrs.is_empty()
        && closure.asyncness.is_none()
        && closure.lifetimes.is_none()
        && closure.constness.is_none()
        && matches!(closure.output, syn::ReturnType::Default))
    .then_some(argument)
}

fn binds_whole(pat: &Pat) -> bool {
    match pat {
        Pat::Ident(ident) => ident.by_ref.is_none() && ident.subpat.is_none(),
        Pat::Type(typed) => binds_whole(&typed.pat),
        _ => false,
    }
}

fn lint_attribute(attr: &Attribute) -> bool {
    ["allow", "expect", "warn", "deny", "forbid"]
        .iter()
        .any(|name| attr.path().is_ident(name))
}

impl<'t> Visit<'t> for Analysis<'t> {
    fn visit_block(&mut self, block: &'t Block) {
        self.scoped(|this| {
            for stmt in &block.stmts {
                this.visit_stmt(stmt);
            }
        });
    }

    fn visit_stmt(&mut self, stmt: &'t Stmt) {
        match stmt {
            Stmt::Local(local) => self.local(local),
            Stmt::Item(item) => self.visit_item(item),
            Stmt::Expr(expr, Some(_)) => {
                if !self.statement(expr) {
                    self.visit_expr(expr);
                }
            }
            Stmt::Expr(expr, None) => self.value(expr),
            Stmt::Macro(mac) => {
                if self.mentions(mac.mac.tokens.clone()) {
                    self.refuse(MACRO);
                }
            }
        }
    }

    fn visit_item(&mut self, item: &'t Item) {
        if let Item::Macro(mac) = item
            && self.mentions(mac.mac.tokens.clone())
        {
            self.refuse(MACRO);
        }
    }

    fn visit_macro(&mut self, mac: &'t Macro) {
        if self.mentions(mac.tokens.clone()) {
            self.refuse(MACRO);
        }
    }

    fn visit_pat(&mut self, _: &'t Pat) {}

    fn visit_arm(&mut self, arm: &'t syn::Arm) {
        self.scoped(|this| {
            this.declare(&arm.pat);
            if let Pat::Guard(guarded) = &arm.pat {
                this.visit_expr(&guarded.guard);
            }
            this.value(&arm.body);
        });
    }

    fn visit_expr(&mut self, expr: &'t Expr) {
        if self.text.attributed(expr) && self.mentions(quote::ToTokens::to_token_stream(expr)) {
            self.refuse(ATTRIBUTE);
            return;
        }
        if self.tracked(expr).is_some() {
            self.refuse(USE);
            return;
        }
        if let Some(op) = self.op(expr) {
            self.refuse(op.shape.misuse());
            return;
        }
        match expr {
            Expr::MethodCall(call) => self.method_call(expr, call),
            Expr::Await(awaited) => self.awaited(awaited),
            Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => {
                if self.borrowed(&unary.expr).is_some() {
                    self.refuse(READ_IN_PLACE);
                } else {
                    syn::visit::visit_expr_unary(self, unary);
                }
            }
            Expr::Call(call) if is_drop(call) => {
                let Some(arg) = call.args.first() else {
                    return;
                };
                if let Some(var) = self.tracked(arg) {
                    self.receiver_moved |= var == RECEIVER;
                    return;
                }
                match self.op(arg) {
                    Some(op) => self.take(&op),
                    None => self.value(arg),
                }
            }
            Expr::Call(call) if call.attrs.is_empty() => {
                self.visit_expr(&call.func);
                for arg in &call.args {
                    self.value(arg);
                }
            }
            Expr::Tuple(tuple) if tuple.attrs.is_empty() => {
                for elem in &tuple.elems {
                    self.value(elem);
                }
            }
            Expr::Array(array) if array.attrs.is_empty() => {
                for elem in &array.elems {
                    self.value(elem);
                }
            }
            Expr::Return(returned) if returned.attrs.is_empty() => {
                if let Some(value) = &returned.expr {
                    self.value(value);
                }
            }
            Expr::Assign(assign) if assign.attrs.is_empty() => {
                self.visit_expr(&assign.left);
                self.value(&assign.right);
            }
            Expr::Binary(binary) if binary.attrs.is_empty() => self.binary(binary),
            Expr::Let(binding) => {
                self.destructured(&binding.expr, &binding.pat);
                self.declare(&binding.pat);
            }
            Expr::Match(matched) => match self.op(&matched.expr) {
                Some(op) => {
                    let pats: Vec<&Pat> = matched.arms.iter().map(|arm| &arm.pat).collect();
                    if !self.patterns(&op, &pats) {
                        self.refuse(RESULT);
                        return;
                    }
                    for arm in &matched.arms {
                        self.visit_arm(arm);
                    }
                }
                None => syn::visit::visit_expr_match(self, matched),
            },
            Expr::If(branch) => {
                self.scoped(|this| {
                    this.visit_expr(&branch.cond);
                    this.visit_block(&branch.then_branch);
                });
                if let Some((_, otherwise)) = &branch.else_branch {
                    self.visit_expr(otherwise);
                }
            }
            Expr::While(looped) => self.scoped(|this| {
                this.visit_expr(&looped.cond);
                this.visit_block(&looped.body);
            }),
            Expr::ForLoop(looped) => {
                self.visit_expr(&looped.expr);
                self.scoped(|this| {
                    this.declare(&looped.pat);
                    this.visit_block(&looped.body);
                });
            }
            Expr::Closure(closure) => self.closure(closure),
            Expr::Async(block) => self.async_block(block),
            _ => syn::visit::visit_expr(self, expr),
        }
    }
}

const RECEIVER: usize = 1;

impl<'t> Analysis<'t> {
    fn method_call(&mut self, expr: &'t Expr, call: &'t ExprMethodCall) {
        if self.consumer(call) {
            return;
        }
        if let Some(op) = self.op(&call.receiver) {
            self.refuse(op.shape.misuse());
            return;
        }
        if call.method == "clone"
            && call.args.is_empty()
            && call.turbofish.is_none()
            && self.read(expr, &call.receiver)
        {
            return;
        }
        if self.half_method(call) {
            return;
        }
        self.visit_expr(&call.receiver);
        for arg in &call.args {
            self.value(arg);
        }
    }

    fn awaited(&mut self, awaited: &'t syn::ExprAwait) {
        if self.kind == Kind::Mpsc
            && awaited.attrs.is_empty()
            && self.method(&awaited.base, "recv", Role::Receiver).is_some()
            && let Expr::MethodCall(call) = &*awaited.base
        {
            self.replace(&call.method, "receive");
            return;
        }
        if self.kind == Kind::Oneshot
            && awaited.attrs.is_empty()
            && self.method(&awaited.base, "closed", Role::Sender).is_some()
        {
            return;
        }
        syn::visit::visit_expr_await(self, awaited);
    }

    fn binary(&mut self, binary: &'t syn::ExprBinary) {
        use syn::BinOp;
        match binary.op {
            BinOp::Add(_)
            | BinOp::Sub(_)
            | BinOp::Mul(_)
            | BinOp::Div(_)
            | BinOp::Rem(_)
            | BinOp::And(_)
            | BinOp::Or(_)
            | BinOp::BitXor(_)
            | BinOp::BitAnd(_)
            | BinOp::BitOr(_)
            | BinOp::Shl(_)
            | BinOp::Shr(_) => {
                self.value(&binary.left);
                self.value(&binary.right);
            }
            op if is_assignment(&op) => {
                self.visit_expr(&binary.left);
                self.value(&binary.right);
            }
            _ => syn::visit::visit_expr_binary(self, binary),
        }
    }

    fn closure(&mut self, closure: &'t syn::ExprClosure) {
        if closure.capture.is_some() {
            self.capture(quote::ToTokens::to_token_stream(&closure.body));
        }
        self.captured += 1;
        self.nested += 1;
        self.scoped(|this| {
            for input in &closure.inputs {
                this.declare(input);
            }
            this.value(&closure.body);
        });
        self.captured -= 1;
        self.nested -= 1;
    }

    fn async_block(&mut self, block: &'t syn::ExprAsync) {
        if block.capture.is_some() {
            self.capture(quote::ToTokens::to_token_stream(&block.block));
        }
        let borrowing = block.capture.is_none();
        self.captured += usize::from(borrowing);
        self.nested += 1;
        self.visit_block(&block.block);
        self.captured -= usize::from(borrowing);
        self.nested -= 1;
    }

    fn capture(&mut self, tokens: TokenStream) {
        fn names(analysis: &Analysis<'_>, tokens: TokenStream) -> bool {
            tokens.into_iter().any(|token| match token {
                TokenTree::Group(group) => names(analysis, group.stream()),
                TokenTree::Ident(ident) => analysis.lookup(&bare(&ident)) == Some(RECEIVER),
                _ => false,
            })
        }
        self.receiver_moved |= names(self, tokens);
    }

    fn half_method(&mut self, call: &'t ExprMethodCall) -> bool {
        let Some(var) = self.tracked(&call.receiver) else {
            return false;
        };
        let method = call.method.to_string();
        match (self.kind, self.role(var), method.as_str()) {
            (Kind::Oneshot, Some(Role::Sender), "is_closed") if call.args.is_empty() => {}
            (
                Kind::Watch,
                Some(Role::Sender),
                "subscribe" | "borrow" | "is_closed" | "closed" | "receiver_count" | "send_replace"
                | "send_modify" | "send_if_modified",
            ) => self.refuse(WATCH_SENDER),
            (Kind::Watch, Some(Role::Receiver), "has_changed") => self.refuse(HAS_CHANGED),
            (Kind::Mpsc, Some(Role::Sender), "is_closed")
            | (Kind::Mpsc, Some(Role::Receiver), "is_closed" | "is_empty" | "len")
                if call.args.is_empty() && call.turbofish.is_none() => {}
            (Kind::Mpsc, Some(Role::Sender), "clone") => self.refuse(SENDER_CLONED),
            (Kind::Mpsc, Some(Role::Sender), "send") => self.refuse(SEND_SHARED),
            (Kind::Mpsc, Some(Role::Receiver), "recv") => self.refuse(RECEIVE_UNAWAITED),
            (Kind::Mpsc, Some(Role::Receiver), "close") => self.refuse(CLOSE_VALUE),
            (Kind::Mpsc, _, _) => self.refuse(MPSC_METHOD),
            _ => self.refuse(USE),
        }
        true
    }

    fn borrowed<'e>(&self, borrowed: &'e Expr) -> Option<Borrow<'e>> {
        if self.kind != Kind::Watch {
            return None;
        }
        let Expr::MethodCall(call) = borrowed else {
            return None;
        };
        let (var, marks_seen) = match self.method(borrowed, "borrow_and_update", Role::Receiver) {
            Some(var) => (var, true),
            None => (self.method(borrowed, "borrow", Role::Receiver)?, false),
        };
        Some(Borrow {
            call,
            var,
            marks_seen,
        })
    }

    fn value(&mut self, expr: &'t Expr) {
        if let Expr::Unary(unary) = expr
            && matches!(unary.op, syn::UnOp::Deref(_))
            && unary.attrs.is_empty()
            && self.read(expr, &unary.expr)
        {
            return;
        }
        self.visit_expr(expr);
    }

    fn read(&mut self, whole: &'t Expr, borrowed: &'t Expr) -> bool {
        let Some(Borrow {
            call,
            var,
            marks_seen,
        }) = self.borrowed(borrowed)
        else {
            return false;
        };
        if !marks_seen {
            self.borrows = true;
            if self.captured > 0 {
                self.refuse(BORROW_CAPTURED);
                return true;
            }
        }
        let Some(receiver) = self.text.slice(&call.receiver) else {
            self.refuse(UNLOCATED);
            return true;
        };
        let replacement = format!("{receiver}.get()");
        self.replace(whole, &replacement);
        if let Some(var) = self.vars.get_mut(var) {
            var.needs_mut = true;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::source::prune_imports;

    fn fix(source: &str) -> (String, Vec<(usize, String, &'static str)>) {
        let mut constructors = BTreeMap::new();
        for (callee, kind) in [
            ("oneshot::channel", Kind::Oneshot),
            ("watch::channel", Kind::Watch),
            ("mpsc::channel", Kind::Mpsc),
        ] {
            for (start, _) in source.match_indices(callee) {
                let mut end = start + callee.len();
                let start = source
                    .get(..start)
                    .and_then(|before| {
                        before
                            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                            .map(|found| found + 1)
                    })
                    .unwrap_or(0);
                let rest = source.get(end..).unwrap_or_default();
                if rest.starts_with("::<") {
                    end += rest.find('>').map_or(0, |close| close + 1);
                }
                constructors.insert(start, (end, kind));
            }
        }
        let outcome = plan(Path::new("lib.rs"), source, &constructors);
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
            .map(|refusal| (refusal.line, refusal.name, refusal.reason))
            .collect();
        (fixed, refusals)
    }

    #[track_caller]
    fn rewrites(before: &str, after: &str) {
        let (fixed, refusals) = fix(before);
        assert_eq!(refusals, [], "{before}");
        assert_eq!(fixed, after);
    }

    #[track_caller]
    fn refused(source: &str, reason: &'static str) {
        let (fixed, refusals) = fix(source);
        assert_eq!(fixed, source);
        assert_eq!(
            refusals.iter().map(|refusal| refusal.2).collect::<Vec<_>>(),
            [reason],
            "{source}"
        );
    }

    #[test]
    fn a_oneshot_channel_becomes_a_reply_pair() {
        rewrites(
            "use tokio::sync::oneshot;\n\nasync fn f() -> Option<u32> {\n    let (tx, mut rx) = oneshot::channel();\n    tx.send(1).unwrap_or(());\n    rx.close();\n    rx.await.ok()\n}\n",
            "async fn f() -> Option<u32> {\n    let (tx, rx) = ::adamantium::Reply::pair();\n    tx.send(1).unwrap_or(());\n    rx.close();\n    rx.receive().await\n}\n",
        );
        rewrites(
            "async fn f() -> u32 {\n    let (tx, rx) = tokio::sync::oneshot::channel::<u32>();\n    drop(tx);\n    rx.await.unwrap_or(7)\n}\n",
            "async fn f() -> u32 {\n    let (tx, rx) = ::adamantium::Reply::<u32>::pair();\n    drop(tx);\n    rx.receive().await.unwrap_or(7)\n}\n",
        );
    }

    #[test]
    fn oneshot_results_read_the_same_as_options() {
        for (before, after) in [
            ("rx.await.is_ok()", "rx.receive().await.is_some()"),
            ("rx.await.is_err()", "rx.receive().await.is_none()"),
            (
                "rx.await.unwrap_or_default()",
                "rx.receive().await.unwrap_or_default()",
            ),
            (
                "rx.await.map_err(|_| 0).unwrap_or(1)",
                "rx.receive().await.ok_or_else(|| 0).unwrap_or(1)",
            ),
            (
                "rx.await.unwrap_or_else(|_| 2)",
                "rx.receive().await.unwrap_or_else(|| 2)",
            ),
            (
                "if let Ok(v) = rx.await { v } else { 0 }",
                "if let Some(v) = rx.receive().await { v } else { 0 }",
            ),
            (
                "match rx.await { Ok(v) if v > 1 => v, Err(_) => 0, _ => 1 }",
                "match rx.receive().await { Some(v) if v > 1 => v, None => 0, _ => 1 }",
            ),
            (
                "{ let Ok(v) = rx.await else { return 0 }; v }",
                "{ let Some(v) = rx.receive().await else { return 0 }; v }",
            ),
            (
                "rx.try_recv().unwrap_or(3)",
                "rx.try_receive().unwrap_or(3)",
            ),
            (
                "rx.try_recv().expect(\"sent\")",
                "rx.try_receive().expect(\"sent\")",
            ),
            (
                "match rx.try_recv() { Ok(v) => v, Err(_) => 0 }",
                "match rx.try_receive() { Ok(v) => v, Err(_) => 0 }",
            ),
        ] {
            rewrites(
                &format!(
                    "async fn f() -> u32 {{\n    let (tx, mut rx) = oneshot::channel();\n    tx.send(5).unwrap_or(());\n    {before}\n}}\n"
                ),
                &format!(
                    "async fn f() -> u32 {{\n    let (tx, rx) = ::adamantium::Reply::pair();\n    tx.send(5).unwrap_or(());\n    {after}\n}}\n"
                ),
            );
        }
    }

    #[test]
    fn oneshot_halves_moved_into_other_locals_and_tasks_follow() {
        rewrites(
            "async fn f() -> bool {\n    let (mut tx, rx) = oneshot::channel::<u8>();\n    let worker = async move {\n        let mut reply = tx;\n        if !reply.is_closed() {\n            reply.closed().await;\n        }\n    };\n    let _ = std::hint::black_box(&worker);\n    let mut answer = rx;\n    answer.close();\n    let _ = answer.try_recv().is_err();\n    std::mem::drop(answer);\n    true\n}\n",
            "async fn f() -> bool {\n    let (tx, rx) = ::adamantium::Reply::<u8>::pair();\n    let worker = async move {\n        let reply = tx;\n        if !reply.is_closed() {\n            reply.closed().await;\n        }\n    };\n    let _ = std::hint::black_box(&worker);\n    let answer = rx;\n    answer.close();\n    let _ = answer.try_receive().is_err();\n    std::mem::drop(answer);\n    true\n}\n",
        );
    }

    #[test]
    fn a_watch_channel_becomes_a_watch_and_a_watcher() {
        rewrites(
            "use tokio::sync::{oneshot, watch};\n\nasync fn f(stop: oneshot::Sender<()>) -> u32 {\n    let (tx, mut rx) = watch::channel(0u32);\n    let _ = tx.send(1);\n    tx.send(2).ok();\n    while rx.changed().await.is_ok() {\n        if rx.borrow_and_update().clone() == 2 {\n            break;\n        }\n    }\n    drop(stop);\n    rx.borrow_and_update().clone()\n}\n",
            "use tokio::sync::oneshot;\n\nasync fn f(stop: oneshot::Sender<()>) -> u32 {\n    let tx = ::adamantium::Watch::new(0u32);\n    let mut rx = tx.subscribe();\n    let _ = tx.send(1);\n    tx.send(2).ok();\n    while rx.changed().await.is_some() {\n        if rx.get() == 2 {\n            break;\n        }\n    }\n    drop(stop);\n    rx.get()\n}\n",
        );
    }

    #[test]
    fn watch_waits_read_the_same_as_options() {
        for (before, after) in [
            ("let _ = rx.changed().await;", "let _ = rx.changed().await;"),
            (
                "rx.changed().await.map_err(|_| 0)?;",
                "rx.changed().await.ok_or_else(|| 0)?;",
            ),
            (
                "if let Ok(()) = rx.changed().await {}",
                "if let Some(()) = rx.changed().await {}",
            ),
            (
                "let Ok(_) = rx.changed().await else { return Ok(()) };",
                "let Some(_) = rx.changed().await else { return Ok(()) };",
            ),
            (
                "match rx.changed().await { Ok(()) => {} Err(_) => {} }",
                "match rx.changed().await { Some(()) => {} None => {} }",
            ),
            (
                "if rx.changed().await.is_err() {}",
                "if rx.changed().await.is_none() {}",
            ),
        ] {
            rewrites(
                &format!(
                    "async fn f() -> Result<(), u8> {{\n    let (tx, mut rx) = watch::channel::<u8>(0);\n    {before}\n    drop(tx);\n    Ok(())\n}}\n"
                ),
                &format!(
                    "async fn f() -> Result<(), u8> {{\n    let tx = ::adamantium::Watch::<u8>::new(0);\n    let mut rx = tx.subscribe();\n    {after}\n    drop(tx);\n    Ok(())\n}}\n"
                ),
            );
        }
    }

    #[test]
    fn watch_clones_keep_their_role_and_borrows_gain_mut() {
        rewrites(
            "fn f() -> (u8, u8) {\n    let (tx, rx) = watch::channel(1u8);\n    let other = tx.clone();\n    drop(other.send(2));\n    let copy = rx.clone();\n    let first = *rx.borrow();\n    (first, copy.borrow().clone())\n}\n",
            "fn f() -> (u8, u8) {\n    let tx = ::adamantium::Watch::new(1u8);\n    let mut rx = tx.subscribe();\n    let other = tx.clone();\n    drop(other.send(2));\n    let mut copy = rx.clone();\n    let first = rx.get();\n    (first, copy.get())\n}\n",
        );
        rewrites(
            "fn f() -> u8 { let (tx, rx) = watch::channel(1u8); let v = *rx.borrow(); drop(tx); v }\n",
            "fn f() -> u8 { let tx = ::adamantium::Watch::new(1u8); let mut rx = tx.subscribe(); let v = rx.get(); drop(tx); v }\n",
        );
    }

    #[test]
    fn a_bounded_mpsc_channel_becomes_a_split_channel() {
        rewrites(
            "use tokio::sync::mpsc;\n\nasync fn f() -> Option<u32> {\n    let (tx, mut rx) = mpsc::channel(4);\n    tx.send(1).await.unwrap_or(());\n    if tx.try_send(2).is_err() {\n        return None;\n    }\n    let open = !tx.is_closed() && !rx.is_closed();\n    rx.close();\n    let first = rx.recv().await;\n    let _ = rx.try_recv().ok();\n    first.filter(|_| open && rx.is_empty() && rx.len() == 0)\n}\n",
            "async fn f() -> Option<u32> {\n    let (tx, rx) = ::adamantium::Channel::bounded(4).split();\n    tx.send(1).await.unwrap_or(());\n    if tx.try_send(2).is_err() {\n        return None;\n    }\n    let open = !tx.is_closed() && !rx.is_closed();\n    rx.close();\n    let first = rx.receive().await;\n    let _ = rx.try_receive().ok();\n    first.filter(|_| open && rx.is_empty() && rx.len() == 0)\n}\n",
        );
        rewrites(
            "async fn f() {\n    let (tx, rx) = tokio::sync::mpsc::channel::<u8>(536_870_911usize);\n    drop(tx);\n    drop(rx);\n}\n",
            "async fn f() {\n    let (tx, rx) = ::adamantium::Channel::<u8>::bounded(536_870_911usize).split();\n    drop(tx);\n    drop(rx);\n}\n",
        );
    }

    #[test]
    fn mpsc_results_read_the_same() {
        for (before, after) in [
            ("let _ = tx.send(5).await;", "let _ = tx.send(5).await;"),
            ("tx.send(5).await.ok();", "tx.send(5).await.ok();"),
            (
                "if tx.send(5).await.is_err() {}",
                "if tx.send(5).await.is_err() {}",
            ),
            (
                "if let Err(_) = tx.send(5).await {}",
                "if let Err(_) = tx.send(5).await {}",
            ),
            (
                "match tx.send(5).await { Ok(()) => {} Err(_) => {} }",
                "match tx.send(5).await { Ok(()) => {} Err(_) => {} }",
            ),
            (
                "tx.send(5).await.map_err(|_| 0)?;",
                "tx.send(5).await.map_err(|_| 0)?;",
            ),
            ("drop(tx.send(5).await);", "drop(tx.send(5).await);"),
            (
                "let _ = tx.try_send(5).unwrap_or_default();",
                "let _ = tx.try_send(5).unwrap_or_default();",
            ),
            (
                "let v = rx.recv().await.unwrap_or(0); drop(v);",
                "let v = rx.receive().await.unwrap_or(0); drop(v);",
            ),
            (
                "while let Some(v) = rx.recv().await { drop(v); }",
                "while let Some(v) = rx.receive().await { drop(v); }",
            ),
            (
                "let Ok(v) = rx.try_recv() else { return Ok(()) }; drop(v);",
                "let Ok(v) = rx.try_receive() else { return Ok(()) }; drop(v);",
            ),
            (
                "match rx.try_recv() { Ok(_) => {} Err(_) => {} }",
                "match rx.try_receive() { Ok(_) => {} Err(_) => {} }",
            ),
            (
                "let _ = rx.try_recv().map_err(|_| 1)?;",
                "let _ = rx.try_receive().map_err(|_| 1)?;",
            ),
        ] {
            rewrites(
                &format!(
                    "async fn f() -> Result<(), u8> {{\n    let (tx, mut rx) = mpsc::channel::<u8>(1);\n    {before}\n    drop(tx);\n    Ok(())\n}}\n"
                ),
                &format!(
                    "async fn f() -> Result<(), u8> {{\n    let (tx, rx) = ::adamantium::Channel::<u8>::bounded(1).split();\n    {after}\n    drop(tx);\n    Ok(())\n}}\n"
                ),
            );
        }
    }

    #[test]
    fn mpsc_halves_moved_into_other_locals_and_tasks_follow() {
        rewrites(
            "async fn f() {\n    let (mut tx, mut rx) = mpsc::channel::<u8>(2);\n    let worker = async move {\n        let mut out = tx;\n        for n in [1, 2] {\n            if out.send(n).await.is_err() {\n                break;\n            }\n        }\n    };\n    let reader = async move {\n        let mut inbox = rx;\n        while let Some(n) = inbox.recv().await {\n            drop(n);\n        }\n    };\n    worker.await;\n    reader.await;\n}\n",
            "async fn f() {\n    let (tx, rx) = ::adamantium::Channel::<u8>::bounded(2).split();\n    let worker = async move {\n        let out = tx;\n        for n in [1, 2] {\n            if out.send(n).await.is_err() {\n                break;\n            }\n        }\n    };\n    let reader = async move {\n        let inbox = rx;\n        while let Some(n) = inbox.receive().await {\n            drop(n);\n        }\n    };\n    worker.await;\n    reader.await;\n}\n",
        );
    }

    #[test]
    fn watch_dereferences_that_copy_the_value_become_get() {
        for (before, after) in [
            ("let v: u8 = *rx.borrow();", "let v: u8 = rx.get();"),
            ("let v = *rx.borrow() + 1;", "let v = rx.get() + 1;"),
            (
                "let mut v = 1; v += *rx.borrow();",
                "let mut v = 1; v += rx.get();",
            ),
            (
                "let v = u8::from(*rx.borrow());",
                "let v = u8::from(rx.get());",
            ),
            (
                "let v = 1u8.max(*rx.borrow());",
                "let v = 1u8.max(rx.get());",
            ),
            ("let v = (*rx.borrow(), 1).0;", "let v = (rx.get(), 1).0;"),
            ("let v = [*rx.borrow()][0];", "let v = [rx.get()][0];"),
            ("let v; v = *rx.borrow();", "let v; v = rx.get();"),
            (
                "let v = match 1 { 1 => *rx.borrow(), _ => 0 };",
                "let v = match 1 { 1 => rx.get(), _ => 0 };",
            ),
            (
                "let v = { *rx.borrow() }; if v > 9 { return *rx.borrow(); }",
                "let v = { rx.get() }; if v > 9 { return rx.get(); }",
            ),
            (
                "drop(*rx.borrow()); let v = 0;",
                "drop(rx.get()); let v = 0;",
            ),
        ] {
            rewrites(
                &format!(
                    "fn f() -> u8 {{\n    let (tx, rx) = watch::channel(1u8);\n    {before}\n    drop(tx);\n    v\n}}\n"
                ),
                &format!(
                    "fn f() -> u8 {{\n    let tx = ::adamantium::Watch::new(1u8);\n    let mut rx = tx.subscribe();\n    {after}\n    drop(tx);\n    v\n}}\n"
                ),
            );
        }
    }

    #[test]
    fn shadowed_names_are_not_the_channel() {
        rewrites(
            "async fn f() -> Option<u8> {\n    let (tx, rx) = oneshot::channel();\n    {\n        let tx = 5u8;\n        println!(\"{tx}\");\n    }\n    let check = |rx: u8| rx + 1;\n    for tx in [1u8] {\n        let _ = check(tx);\n    }\n    if let Some(rx) = Some(2u8) {\n        let _ = check(rx);\n    }\n    tx.send(1).unwrap_or(());\n    rx.await.ok()\n}\n",
            "async fn f() -> Option<u8> {\n    let (tx, rx) = ::adamantium::Reply::pair();\n    {\n        let tx = 5u8;\n        println!(\"{tx}\");\n    }\n    let check = |rx: u8| rx + 1;\n    for tx in [1u8] {\n        let _ = check(tx);\n    }\n    if let Some(rx) = Some(2u8) {\n        let _ = check(rx);\n    }\n    tx.send(1).unwrap_or(());\n    rx.receive().await\n}\n",
        );
    }

    #[test]
    fn uses_whose_meaning_would_change_are_refused() {
        for (body, reason) in [
            (
                "let (tx, rx) = watch::channel(None); drop(rx); tx.send(Some(String::new())).ok(); drop(tx);",
                WATCH_UNWATCHED,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let watcher = rx; drop(watcher); tx.send(1).ok();",
                WATCH_UNWATCHED,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let task = async move { let _v = *rx.borrow(); }; task.await; tx.send(1).ok();",
                WATCH_UNWATCHED,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let task = async move { tx.send(1).ok(); }; task.await; let _v = *rx.borrow();",
                WATCH_UNWATCHED,
            ),
            (
                "let (tx, mut rx) = watch::channel(0u8); rx.changed().await.map_err(move |_| { let copy = rx.clone(); drop(copy); }).ok(); tx.send(1).ok();",
                WATCH_UNWATCHED,
            ),
            (
                "let (tx, mut rx) = watch::channel(0u8); rx.changed().await.unwrap(); drop(tx);",
                RESULT,
            ),
            (
                "let (tx, mut rx) = watch::channel(0u8); rx.changed().await.expect(\"sender\"); drop(tx);",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); tx.send(1).unwrap(); drop(rx);",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); drop(tx); let _ = rx.await.expect(\"reply\");",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); tx.send(1).unwrap_or(()); let _ = rx.await?;",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); let r = &rx; drop((tx, r));",
                USE,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); drop(tx); let _v = rx.await;",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); drop(tx); match rx.await { Ok(_) => {} Err(e) => drop(e) }",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); let _ = tx.send(1).map_err(|v| v); drop(rx);",
                RESULT,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); keep(tx); drop(rx);",
                USE,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); println!(\"{tx:?}\"); drop(rx);",
                MACRO,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); assert!(tx.send(1).is_ok()); drop(rx);",
                MACRO,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); #[cfg(test)] drop(tx); drop(rx);",
                ATTRIBUTE,
            ),
            ("let (tx, _) = oneshot::channel::<u8>(); drop(tx);", UNBOUND),
            ("let pair = oneshot::channel::<u8>(); drop(pair);", UNBOUND),
            (
                "let (tx, rx): (oneshot::Sender<u8>, _) = oneshot::channel(); drop((tx, rx));",
                TYPED,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); if tx.send(1).is_err() { drop(rx); }",
                WATCH_SEND,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); tx.send(1).unwrap(); drop(rx);",
                WATCH_SEND,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = tx.subscribe(); drop(rx);",
                WATCH_SENDER,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = *tx.borrow(); drop(rx);",
                WATCH_SENDER,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = rx.has_changed(); drop(tx);",
                HAS_CHANGED,
            ),
            (
                "let (tx, mut rx) = watch::channel(0u8); let _ = rx.changed().await; let _v = *rx.borrow(); drop(tx);",
                BORROW_SEEN,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let read = || *rx.borrow(); let _ = read(); drop(tx);",
                BORROW_CAPTURED,
            ),
            (
                "let (tx, mut rx) = watch::channel(0u8); let v = rx.changed().await.unwrap(); drop((tx, v));",
                RESULT,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = rx.borrow().len(); drop(tx);",
                USE,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = *rx.borrow() == 1; drop(tx);",
                READ_IN_PLACE,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _ = *rx.borrow(); drop(tx);",
                READ_IN_PLACE,
            ),
            (
                "let (tx, rx) = watch::channel((0u8, 1u8)); let _v = (*rx.borrow()).0; drop(tx);",
                READ_IN_PLACE,
            ),
            (
                "let (tx, rx) = watch::channel(0u8); let _v = &*rx.borrow(); drop(tx);",
                READ_IN_PLACE,
            ),
            (
                "let (tx, rx) = watch::channel((0u8, 1u8)); let (_c, _) = *rx.borrow(); drop(tx);",
                READ_IN_PLACE,
            ),
            (
                "let (tx, rx) = oneshot::channel::<u8>(); drop(tx); let waiter = async { rx.await.ok() }; let _ = waiter.await;",
                AWAIT_CAPTURED,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(0); drop((tx, rx));",
                CAPACITY,
            ),
            (
                "let n = 4; let (tx, rx) = mpsc::channel::<u8>(n); drop((tx, rx));",
                CAPACITY,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(536_870_912); drop((tx, rx));",
                CAPACITY,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(4u8); drop((tx, rx));",
                CAPACITY,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let other = tx.clone(); drop(other); drop(tx); drop(rx);",
                SENDER_CLONED,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let sending = tx.send(1); drop(sending); drop(rx);",
                SEND_SHARED,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let both = async { tx.send(1).await.ok() }; let _ = both.await; drop(rx);",
                SEND_SHARED,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let send = async |v| tx.send(v).await.is_ok(); let _ = send(1).await; drop(rx);",
                SEND_SHARED,
            ),
            (
                "let (tx, mut rx) = mpsc::channel::<u8>(1); drop(tx); let next = rx.recv(); let _ = next.await;",
                RECEIVE_UNAWAITED,
            ),
            (
                "let (tx, mut rx) = mpsc::channel::<u8>(1); drop(tx); let closed = rx.close(); drop(closed);",
                CLOSE_VALUE,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let _ = tx.reserve().await; drop(rx);",
                MPSC_METHOD,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let _ = tx.capacity(); drop(rx);",
                MPSC_METHOD,
            ),
            (
                "let (tx, mut rx) = mpsc::channel::<u8>(1); drop(tx); let _ = rx.blocking_recv();",
                MPSC_METHOD,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); tx.send(1).await.unwrap(); drop(rx);",
                RESULT,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); tx.send(1).await?; drop(rx);",
                RESULT,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); let sent = tx.send(1).await; drop((sent, rx));",
                RESULT,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); tx.try_send(1).expect(\"room\"); drop(rx);",
                RESULT,
            ),
            (
                "let (tx, mut rx) = mpsc::channel::<u8>(1); drop(tx); let _ = rx.try_recv().unwrap();",
                RESULT,
            ),
            (
                "let (tx, rx) = mpsc::channel::<u8>(1); match tx.try_send(1) { Err(TrySendError::Full(_)) => {} _ => {} } drop(rx);",
                RESULT,
            ),
        ] {
            refused(
                &format!(
                    "fn keep<T>(_: T) {{}}\nasync fn f() -> Result<(), tokio::sync::oneshot::error::RecvError> {{\n    {body}\n    Ok(())\n}}\n"
                ),
                reason,
            );
        }
    }

    #[test]
    fn functions_an_attribute_macro_might_rewrite_are_refused() {
        refused(
            "#[instrument]\nasync fn f() {\n    let (tx, rx) = oneshot::channel::<u8>();\n    drop((tx, rx));\n}\n",
            ATTRIBUTE_MACRO,
        );
        refused(
            "#[async_trait]\nimpl Run for Job {\n    async fn run(&self) {\n        let (tx, rx) = oneshot::channel::<u8>();\n        drop(tx);\n        drop(rx);\n    }\n}\n",
            ATTRIBUTE_MACRO,
        );
        rewrites(
            "#[tokio::main(flavor = \"current_thread\")]\nasync fn main() {\n    let (tx, rx) = oneshot::channel::<u8>();\n    drop(tx);\n    drop(rx);\n}\n",
            "#[tokio::main(flavor = \"current_thread\")]\nasync fn main() {\n    let (tx, rx) = ::adamantium::Reply::<u8>::pair();\n    drop(tx);\n    drop(rx);\n}\n",
        );
    }

    #[test]
    fn refusals_name_the_channel_and_its_line() {
        let (fixed, refusals) = fix(
            "fn f() {\n    let (tx, rx) = oneshot::channel::<u8>();\n    keep(rx);\n    drop(tx);\n    keep(oneshot::channel::<u8>());\n}\n",
        );
        assert!(!fixed.contains("adamantium"), "{fixed}");
        assert_eq!(
            refusals,
            [
                (2, "(tx, rx)".to_owned(), USE),
                (5, "oneshot::channel::<u8>(..)".to_owned(), UNBOUND),
            ]
        );
    }

    #[test]
    fn imports_still_named_stay() {
        rewrites(
            "use tokio::sync::{oneshot, watch};\nfn f(_: watch::Receiver<u8>) {\n    let (tx, rx) = oneshot::channel::<u8>();\n    drop(tx);\n    drop(rx);\n}\n",
            "use tokio::sync::watch;\nfn f(_: watch::Receiver<u8>) {\n    let (tx, rx) = ::adamantium::Reply::<u8>::pair();\n    drop(tx);\n    drop(rx);\n}\n",
        );
    }

    #[test]
    fn rewritten_code_is_left_alone() {
        let source = "fn f() {\n    let (tx, rx) = ::adamantium::Reply::<u8>::pair();\n    drop(tx);\n    drop(rx);\n}\n";
        rewrites(source, source);
    }
}
