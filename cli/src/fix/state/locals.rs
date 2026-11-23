use syn::{Expr, Local, Pat, visit::Visit};

use super::{Group, LockKind, NAME_TAKEN, Reason, Walker};
use crate::fix::{
    Edit,
    source::{bare, pattern_names},
};

impl<'a> Walker<'a> {
    pub(super) fn local_statement(&mut self, local: &'a Local) {
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
                self.settle_awaiting(local, Some(var));
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
        self.settle_awaiting(local, ident.and(declared));
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
        self.settle_awaiting(local, Some(var));
        var
    }

    fn settle_awaiting(&mut self, local: &'a Local, var: Option<usize>) {
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
