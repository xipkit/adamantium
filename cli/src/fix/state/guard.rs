use std::{collections::BTreeSet, ops::Range};

use syn::{BinOp, Expr, Lit, Local, Macro, Pat, Stmt, UnOp, visit::Visit};

use super::{
    ATTRIBUTED, BlockKind, LockEvidence, LockKind, Node, OUTSIDE_STATEMENT, Reason, UNLOCATED,
    Walker, held::Held, resolve::Resolved,
};
use crate::fix::{
    Edit,
    source::{is_assignment, names_in},
};

#[derive(Clone, Copy)]
enum Root<'a> {
    Statement { full: Full<'a>, semi: bool },
    Let(&'a Local, &'a Expr),
    Value(&'a Expr),
    Condition(&'a Expr),
}

#[derive(Clone, Copy)]
pub(super) enum Full<'a> {
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
pub(super) enum Mode {
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

struct Plan<'a> {
    root: Root<'a>,
    narrow: Option<&'a Expr>,
    mode: Mode,
    receiver: bool,
}

pub(super) fn is(expr: &Expr, child: &Expr) -> bool {
    std::ptr::eq(expr, child)
}

fn is_comparison(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq(_) | BinOp::Ne(_) | BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_)
    )
}

impl<'a> Walker<'a> {
    pub(super) fn guard_access<'e>(
        &self,
        expr: &'e Expr,
    ) -> Option<(usize, &'e Expr, Option<Mode>)> {
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

    pub(super) fn unwrapped(&self, expr: &Expr) -> Option<(usize, String)> {
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

    pub(super) fn dropped(&self, expr: &Expr) -> Option<usize> {
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

    fn plan_guard(&self, guard: &'a Expr) -> Result<Plan<'a>, Reason> {
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

    pub(super) fn guard(&mut self, var: usize, guard: &'a Expr, held: &Expr, access: Option<Mode>) {
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
        let plan = self.plan_guard(guard)?;
        let (bare_name, name) = self
            .vars
            .get(var)
            .map(|var| (var.name.clone(), var.spelling.clone()))
            .ok_or(UNLOCATED)?;
        let holder = self.holder(var, held).ok_or(UNLOCATED)?;
        let base = self.place_base(held);
        let guard_range = self.text.of(guard).ok_or(UNLOCATED)?;
        let full = plan.root.full();
        let mut scan = Held::new(self, guard);
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
                let mut scan = Held::new(self, guard);
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
        let Held { runs, .. } = inside;
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
}

pub(super) fn callee_name(func: &Expr) -> Option<String> {
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
