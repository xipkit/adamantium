use std::{collections::BTreeSet, ops::Range};

use syn::{Expr, GenericArgument, PathArguments, Type};

use super::{
    LockEvidence, LockKind, Walker,
    resolve::{ARC, ARC_CRATES, plain_names},
};

const ACTOR: &str = "::adamantium::Actor";

impl<'a> Walker<'a> {
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

    pub(super) fn actor_of(&self, state: &Type) -> Option<String> {
        Some(format!("{ACTOR}<{}>", self.text.slice(state)?))
    }

    pub(super) fn arc_state<'t>(&self, ty: &'t Type) -> Option<(LockKind, &'t Type)> {
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

    pub(super) fn lock_state<'t>(&self, ty: &'t Type) -> Option<(LockKind, &'t Type)> {
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

    pub(super) fn constructor(&self, expr: &Expr) -> Option<(LockKind, String)> {
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

    pub(super) fn clone_of(&self, expr: &Expr) -> Option<(usize, Option<String>)> {
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
}

pub(super) fn is_unsized(ty: &Type, aliases: &BTreeSet<String>) -> bool {
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
