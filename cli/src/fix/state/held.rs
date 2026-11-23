use std::collections::BTreeSet;

use syn::{BinOp, Expr, ExprLit, Item, Lit, Local, Macro, visit::Visit};

use super::{
    Walker,
    guard::{Full, callee_name, is},
};
use crate::fix::source::{bare, format_captures};

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

pub(super) struct Held<'w, 'a> {
    walker: &'w Walker<'a>,
    guard: &'a Expr,
    deferred: usize,
    branch: usize,
    pub(super) awaits: bool,
    pub(super) locks: bool,
    pub(super) leaves: bool,
    pub(super) calls: bool,
    pub(super) unfollowed: Option<&'static str>,
    pub(super) runs: BTreeSet<String>,
}

impl<'w, 'a> Held<'w, 'a> {
    pub(super) fn new(walker: &'w Walker<'a>, guard: &'a Expr) -> Self {
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

    pub(super) fn full(&mut self, full: Full<'a>) {
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

impl<'a> Visit<'a> for Held<'_, 'a> {
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
