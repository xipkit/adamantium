use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    path::Path,
};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::{BinOp, Block, Expr, Item, Lit, Local, Macro, Pat, Stmt, visit::Visit};

use super::{
    Edit,
    scan::{Crate, macro_attribute},
    source::{Text, bare, format_captures, item_attributes, pattern_names, string_literal},
};
use guard::is;
use macros::{MacroArgs, Parsed};
use resolve::{Binding, ItemScope, Names};
use settle::{Draft, Followed, refuse_overlaps};

mod calls;
mod fields;
mod guard;
mod held;
mod locals;
mod locks;
mod macros;
mod resolve;
mod settle;
#[cfg(test)]
mod tests;

pub(super) use settle::settle;

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
}

impl<'a> Walker<'a> {
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
