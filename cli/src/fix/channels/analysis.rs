use std::{ops::Range, path::Path};

use proc_macro2::{TokenStream, TokenTree};
use syn::{Attribute, Block, Expr, ExprMethodCall, Item, Local, Macro, Pat, Stmt, visit::Visit};

use super::{
    ATTRIBUTE, AWAIT_CAPTURED, BORROW_CAPTURED, CLOSE_VALUE, HAS_CHANGED, Kind, MACRO, MPSC_METHOD,
    READ_IN_PLACE, RECEIVE_UNAWAITED, RESULT, SEND_SHARED, SENDER_CLONED, UNLOCATED, USE,
    WATCH_SEND, WATCH_SENDER,
};
use crate::fix::{
    Edit,
    source::{Text, bare, format_captures, is_assignment, pattern_names, string_literal},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Sender,
    Receiver,
}

pub(super) struct Var {
    pub(super) role: Role,
    pub(super) ident: Range<usize>,
    pub(super) mutable: Option<Range<usize>>,
    pub(super) needs_mut: bool,
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

pub(super) struct Analysis<'t> {
    pub(super) text: &'t Text<'t>,
    pub(super) file: &'t Path,
    pub(super) kind: Kind,
    pub(super) vars: Vec<Var>,
    pub(super) scopes: Vec<Vec<(String, Option<usize>)>>,
    pub(super) edits: Vec<Edit>,
    pub(super) refusal: Option<&'static str>,
    pub(super) captured: usize,
    pub(super) nested: usize,
    pub(super) sends: bool,
    pub(super) sends_nested: bool,
    pub(super) receiver_moved: bool,
    pub(super) waits: bool,
    pub(super) borrows: bool,
}

impl<'t> Analysis<'t> {
    pub(super) fn edit(&self, range: Range<usize>, replacement: String) -> Edit {
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

    pub(super) fn refuse(&mut self, reason: &'static str) {
        self.refusal.get_or_insert(reason);
    }

    pub(super) fn var(&mut self, role: Role, ident: &syn::PatIdent) -> usize {
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
