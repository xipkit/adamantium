use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use syn::{Expr, ExprField, ExprStruct, Item, Member, Pat, Type, visit::Visit};

use super::{Group, ItemScope, Reason, Resolved, Var, Walker, bare};
use crate::fix::scan::{idents, macro_attribute};

pub(super) struct Field {
    pub(super) structure: String,
    pub(super) var: usize,
}

impl<'a> Walker<'a> {
    pub(super) fn candidates(&mut self, tree: &'a syn::File) {
        self.module_candidates(&tree.items);
    }

    fn module_candidates(&mut self, items: &'a [Item]) {
        for item in items {
            match item {
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        self.items.push(ItemScope::new(items, true));
                        self.module_candidates(items);
                        self.items.pop();
                    }
                }
                Item::Struct(structure) => {
                    self.generic(&structure.generics, |walker| {
                        walker.struct_candidates(structure);
                    });
                }
                _ => {}
            }
        }
    }

    fn struct_candidates(&mut self, structure: &'a syn::ItemStruct) {
        let syn::Fields::Named(named) = &structure.fields else {
            return;
        };
        let name = bare(&structure.ident);
        for field in &named.named {
            let Some(ident) = &field.ident else {
                continue;
            };
            let Some((kind, state)) = self.arc_state(&field.ty) else {
                continue;
            };
            let (Some(range), Some(actor)) = (self.text.of(&field.ty), self.actor_of(state)) else {
                continue;
            };
            let member = bare(ident);
            let mut group = Group::new(
                format!("{name}.{member}"),
                self.text.line(ident.span()),
                kind,
                vec![self.edit(range, actor)],
            );
            group.member = Some(member.clone());
            if !matches!(field.vis, syn::Visibility::Inherited) {
                group.refuse(Reason::Unfollowed(
                    "code outside its module might use it, since it is visible there",
                ));
            }
            if field.attrs.iter().any(|attr| !attr.path().is_ident("doc")) {
                group.refuse(Reason::Unfollowed("it has attributes"));
            }
            if macro_attribute(&structure.attrs) {
                group.refuse(Reason::Unfollowed(
                    "its struct has an attribute or a derive other than `Clone` and `Default`",
                ));
            }
            if may_be_unsized(state, &structure.generics) {
                group.refuse(Reason::Unfollowed(
                    "its state might be unsized, which an actor can't hold",
                ));
            }
            if self.names.declared.get(&name).copied() != Some(1) {
                group.refuse(Reason::Unfollowed(
                    "another item or import in the file has its struct's name",
                ));
            }
            let index = self.groups.len();
            self.groups.push(group);
            let var = self.vars.len();
            self.vars.push(Var {
                name: member.clone(),
                spelling: ident.to_string(),
                group: Some(index),
                used: true,
                field: true,
                lock_ref: false,
                record: None,
            });
            self.fields.insert(
                member,
                Field {
                    structure: name.clone(),
                    var,
                },
            );
        }
    }

    fn structure(&self, name: &syn::Ident) -> Option<String> {
        let name = bare(name);
        (self.fields.values().any(|field| field.structure == name)
            && self.names.declared.get(&name).copied() == Some(1)
            && matches!(self.resolve_name(&name), Resolved::Item))
        .then_some(name)
    }

    pub(super) fn literal_struct(&self, path: &syn::Path) -> Option<String> {
        if path.leading_colon.is_some() || path.segments.len() != 1 {
            return None;
        }
        let ident = &path.segments.first()?.ident;
        if ident == "Self" {
            self.impls.last().cloned().flatten()
        } else {
            self.structure(ident)
        }
    }

    pub(super) fn record_type(&self, ty: &Type) -> Option<String> {
        match ty {
            Type::Reference(reference) => self.record_type(&reference.elem),
            Type::Paren(inner) => self.record_type(&inner.elem),
            Type::Path(path) if path.qself.is_none() => self.literal_struct(&path.path),
            _ => None,
        }
    }

    pub(super) fn impl_struct(&self, item: &syn::ItemImpl) -> Option<String> {
        match &*item.self_ty {
            Type::Path(path) if path.qself.is_none() => self.literal_struct(&path.path),
            _ => None,
        }
    }

    pub(super) fn record(&mut self, pat: &Pat, ty: &Type) {
        let Pat::Ident(ident) = pat else {
            return;
        };
        if ident.by_ref.is_some() || ident.subpat.is_some() {
            return;
        }
        let record = self.record_type(ty);
        if let Some(var) = self.lookup(&bare(&ident.ident))
            && let Some(entry) = self.vars.get_mut(var)
        {
            entry.record = record;
        }
    }

    pub(super) fn candidate(&self, member: &Member) -> Option<&Field> {
        match member {
            Member::Named(name) => self.fields.get(&bare(name)),
            Member::Unnamed(_) => None,
        }
    }

    fn traced(&self, base: &Expr, structure: &str) -> bool {
        let Expr::Path(path) = base else {
            return false;
        };
        if path.qself.is_some() || !path.attrs.is_empty() {
            return false;
        }
        let Some(ident) = path.path.get_ident() else {
            return false;
        };
        if ident == "self" {
            return self
                .impls
                .last()
                .is_some_and(|of| of.as_deref() == Some(structure));
        }
        self.lookup(&bare(ident))
            .and_then(|var| self.vars.get(var))
            .is_some_and(|var| var.record.as_deref() == Some(structure))
    }

    pub(super) fn field_place(&self, expr: &Expr) -> Option<usize> {
        let Expr::Field(access) = expr else {
            return None;
        };
        let field = self.candidate(&access.member)?;
        (access.attrs.is_empty() && self.traced(&access.base, &field.structure))
            .then_some(field.var)
    }

    pub(super) fn place_base(&self, expr: &Expr) -> Option<String> {
        let Expr::Field(access) = expr else {
            return None;
        };
        let Expr::Path(path) = &*access.base else {
            return None;
        };
        self.field_place(expr)?;
        path.path.get_ident().map(bare)
    }

    pub(super) fn literal(&mut self, literal: &'a ExprStruct) {
        let structure = if literal.qself.is_none() {
            self.literal_struct(&literal.path)
        } else {
            None
        };
        for value in &literal.fields {
            let Some((owner, var)) = self
                .candidate(&value.member)
                .map(|field| (field.structure.clone(), field.var))
            else {
                self.visit_expr(&value.expr);
                continue;
            };
            if structure.as_deref() != Some(owner.as_str()) || !value.attrs.is_empty() {
                self.refuse(
                    var,
                    Reason::Unfollowed("a struct literal fix can't trace to its struct sets it"),
                );
                self.visit_expr(&value.expr);
                continue;
            }
            self.initializer(var, &value.expr);
        }
        if let Some(rest) = &literal.rest {
            self.visit_expr(rest);
        }
    }

    fn initializer(&mut self, var: usize, expr: &'a Expr) {
        if let Some((_, constructor)) = self.constructor(expr)
            && let Some(range) = self.text.of(expr)
        {
            self.visit_expr(expr);
            let edit = self.edit(range, constructor);
            self.add_edit(var, edit);
            return;
        }
        let Some(group) = self.group_of(var) else {
            self.visit_expr(expr);
            return;
        };
        if let Some((source, replacement)) = self.clone_of(expr) {
            self.flows(group, source, expr, replacement);
            return;
        }
        if let Some(returned) = self.returned_by(expr) {
            self.links.push((group, returned));
            return;
        }
        self.refuse(
            var,
            Reason::Unfollowed("a struct literal sets it to a value fix can't follow"),
        );
        self.visit_expr(expr);
    }

    pub(super) fn stray(&mut self, access: &ExprField) {
        let Some(field) = self.candidate(&access.member) else {
            return;
        };
        let var = field.var;
        let reason = if self.traced(&access.base, &field.structure) {
            "it is borrowed, passed or stored where fix can't follow it"
        } else {
            "fix can't trace a value with a field of its name to its struct"
        };
        self.refuse(var, Reason::Unfollowed(reason));
    }

    pub(super) fn mention_field(&mut self, name: &str, reason: Reason) {
        if let Some(var) = self.fields.get(name).map(|field| field.var) {
            self.refuse(var, reason);
        }
    }

    pub(super) fn attribute_macro(&mut self, tokens: TokenStream) {
        let mut names = BTreeSet::<String>::new();
        idents(tokens, &mut names);
        for name in names {
            let reason = Reason::Unfollowed("an attribute macro reads code that names it");
            self.mention_field(&name, reason);
            self.mention_callee(&name, reason);
        }
    }

    pub(super) fn destructures(&mut self, pat: &Pat) {
        struct Members(Vec<String>);
        impl<'ast> Visit<'ast> for Members {
            fn visit_field_pat(&mut self, field: &'ast syn::FieldPat) {
                if let Member::Named(name) = &field.member {
                    self.0.push(bare(name));
                }
                syn::visit::visit_field_pat(self, field);
            }
        }
        let mut members = Members(Vec::new());
        members.visit_pat(pat);
        for name in members.0 {
            self.mention_field(&name, Reason::Unfollowed("a pattern takes it apart"));
        }
    }

    pub(super) fn assigns(&mut self, left: &Expr) {
        struct Members(Vec<String>);
        impl<'ast> Visit<'ast> for Members {
            fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
                if let Member::Named(name) = &field.member {
                    self.0.push(bare(name));
                }
                syn::visit::visit_field_value(self, field);
            }
        }
        let mut members = Members(Vec::new());
        members.visit_expr(left);
        for name in members.0 {
            self.mention_field(
                &name,
                Reason::Unfollowed("a destructuring assignment takes it apart"),
            );
        }
    }

    pub(super) fn lock_site(&mut self, var: usize, runs: BTreeSet<String>) {
        let Some(group) = self.group_of(var) else {
            return;
        };
        let function = self.functions.last().cloned();
        let mut block = false;
        let mut boundary = None;
        let deferred = function.as_ref().is_none_or(|(_, depth)| {
            self.stack
                .iter()
                .enumerate()
                .skip(*depth)
                .any(|(index, node)| match node {
                    super::Node::Expr(Expr::Closure(_)) => true,
                    super::Node::Expr(Expr::Async(expr)) => {
                        block = true;
                        let unproved = !expr.attrs.is_empty()
                            || !matches!(index.checked_sub(1).and_then(|i| self.stack.get(i)),
                                Some(super::Node::Expr(Expr::Await(await_))) if await_.attrs.is_empty());
                        if unproved {
                            boundary = Some(format!("future boundary `async block` at line {} is not directly awaited; the rewrite remains manual", expr.async_token.span.start().line));
                        }
                        unproved
                    }
                    _ => false,
                })
        });
        if let Some(group) = self.groups.get_mut(group) {
            group.callbacks.extend(runs);
            if boundary.is_some() {
                group.refuse(Reason::Poll);
                group.boundary = boundary;
            }
            match function {
                Some((name, _)) if !deferred => {
                    if block {
                        group.poll_blocks.insert(name.clone());
                    }
                    group.functions.insert(name);
                }
                _ => group.escapes = true,
            }
        }
    }
}

pub(super) fn may_be_unsized(state: &Type, generics: &syn::Generics) -> bool {
    let params: BTreeSet<String> = generics
        .type_params()
        .map(|param| param.ident.to_string())
        .collect();
    let relaxed = |bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>| {
        bounds.iter().any(
            |bound| matches!(bound, syn::TypeParamBound::Trait(bound) if bound.maybe.is_some()),
        )
    };
    let mut unsized_params: BTreeSet<String> = generics
        .type_params()
        .filter(|param| relaxed(&param.bounds))
        .map(|param| param.ident.to_string())
        .collect();
    for predicate in generics
        .where_clause
        .iter()
        .flat_map(|clause| &clause.predicates)
    {
        if let syn::WherePredicate::Type(predicate) = predicate
            && relaxed(&predicate.bounds)
            && let Type::Path(path) = &predicate.bounded_ty
            && let Some(ident) = path.path.get_ident()
        {
            unsized_params.insert(ident.to_string());
        }
    }
    match state {
        Type::Paren(inner) => may_be_unsized(&inner.elem, generics),
        Type::Group(inner) => may_be_unsized(&inner.elem, generics),
        Type::Path(path) => {
            path.qself.is_some()
                || path
                    .path
                    .get_ident()
                    .is_some_and(|ident| unsized_params.contains(&ident.to_string()))
                || (path.path.segments.len() > 1
                    && path.path.leading_colon.is_none()
                    && path
                        .path
                        .segments
                        .first()
                        .is_some_and(|first| params.contains(&first.ident.to_string())))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::super::{
        Reason, plan, settle,
        tests::{fix, mutex_spans},
    };
    use crate::fix::scan::Crate;

    const MUTEXES: &[&str] = &["Mutex", "Mutex<u64>", "Mutex<Vec<u8>>"];

    const COUNTER: &str = "use std::sync::{Arc, Mutex};\n#[derive(Clone, Default)]\nstruct Counter {\n    hits: Arc<Mutex<u64>>,\n    name: String,\n}\n";

    fn refused(body: &str, reason: Reason) {
        let source = format!("{COUNTER}{body}\n");
        let (fixed, refusals) = fix(&source, MUTEXES);
        assert_eq!(fixed, source);
        assert!(
            refusals.contains(&("Counter.hits".to_owned(), reason)),
            "{refusals:?}\n{source}"
        );
    }

    #[test]
    fn a_field_and_its_clones_become_an_actor() {
        let (fixed, refusals) = fix(
            &format!(
                "{COUNTER}impl Counter {{\n    fn new() -> Self {{\n        Self {{ hits: Arc::new(Mutex::new(0)), name: String::new() }}\n    }}\n    fn hit(&self) {{\n        *self.hits.lock().unwrap() += 1;\n    }}\n    fn share(&self) -> Self {{\n        Self {{ hits: Arc::clone(&self.hits), name: self.name.clone() }}\n    }}\n}}\nfn total(counter: &Counter) -> u64 {{\n    let handle = counter.hits.clone();\n    let total = *handle.lock().unwrap();\n    total\n}}\nfn make() -> Counter {{\n    let hits = Arc::new(Mutex::new(5));\n    let counter = Counter {{ hits, name: String::new() }};\n    let n = *counter.hits.lock().expect(\"hits\");\n    drop(n);\n    counter\n}}\n"
            ),
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "#[derive(Clone, Default)]\nstruct Counter {\n    hits: ::adamantium::Actor<u64>,\n    name: String,\n}\nimpl Counter {\n    fn new() -> Self {\n        Self { hits: ::adamantium::Actor::new(0), name: String::new() }\n    }\n    fn hit(&self) {\n        self.hits.with(|hits| *hits += 1);\n    }\n    fn share(&self) -> Self {\n        Self { hits: self.hits.clone(), name: self.name.clone() }\n    }\n}\nfn total(counter: &Counter) -> u64 {\n    let handle = counter.hits.clone();\n    let total = handle.read(|handle| *handle);\n    total\n}\nfn make() -> Counter {\n    let hits = ::adamantium::Actor::new(5);\n    let counter = Counter { hits, name: String::new() };\n    let n = counter.hits.read(|hits| *hits);\n    drop(n);\n    counter\n}\n"
        );
    }

    #[test]
    fn a_local_stored_in_a_field_stands_or_falls_with_it() {
        let source = format!(
            "{COUNTER}fn consume<T>(_: T) {{}}\nfn make() -> Counter {{\n    let hits = Arc::new(Mutex::new(5));\n    consume(&hits);\n    Counter {{ hits, name: String::new() }}\n}}\nfn hit(counter: &Counter) {{\n    *counter.hits.lock().unwrap() += 1;\n}}\n"
        );
        let (fixed, refusals) = fix(&source, MUTEXES);
        assert_eq!(fixed, source);
        assert!(
            refusals.contains(&(
                "Counter.hits".to_owned(),
                Reason::Unfollowed("it holds the same state as a value fix leaves for review")
            )),
            "{refusals:?}"
        );
        assert!(
            refusals.iter().any(|(name, _)| name == "hits"),
            "{refusals:?}"
        );
    }

    #[test]
    fn fields_code_outside_the_module_might_use_are_refused() {
        for source in [
            "use std::sync::{Arc, Mutex};\nstruct Counter { pub hits: Arc<Mutex<u64>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Counter { pub(crate) hits: Arc<Mutex<u64>> }\n",
            "use std::sync::{Arc, Mutex};\n#[derive(Debug)]\nstruct Counter { hits: Arc<Mutex<u64>> }\n",
            "use std::sync::{Arc, Mutex};\n#[repr(C)]\nstruct Counter { hits: Arc<Mutex<u64>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Counter { #[cfg(test)] hits: Arc<Mutex<u64>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Counter { hits: Arc<Mutex<u64>> }\nmod inner { struct Counter; }\n",
            "use std::sync::{Arc, Mutex};\nstruct Counter { hits: Arc<Mutex<u64>> }\nstruct Other { hits: u8 }\n",
            "use std::sync::{Arc, Mutex};\nstruct Counter { hits: Arc<Mutex<u64>> }\nenum Other { Hits { hits: u8 } }\n",
        ] {
            let (fixed, refusals) = fix(source, MUTEXES);
            assert_eq!(fixed, source);
            assert!(
                refusals.iter().any(|(name, reason)| name == "Counter.hits"
                    && matches!(reason, Reason::Unfollowed(_))),
                "{refusals:?}\n{source}"
            );
        }
    }

    #[test]
    fn uses_fix_cannot_follow_refuse_the_field() {
        for (body, reason) in [
            (
                "fn consume<T>(_: T) {}\nimpl Counter { fn f(&self) { consume(&self.hits); } }",
                "it is borrowed, passed or stored where fix can't follow it",
            ),
            (
                "fn f(counter: Box<Counter>) { *counter.hits.lock().unwrap() += 1; }",
                "fix can't trace a value with a field of its name to its struct",
            ),
            (
                "fn f(counter: &Counter) { let n = counter.hits.try_lock().is_ok(); drop(n); }",
                "it is borrowed, passed or stored where fix can't follow it",
            ),
            (
                "fn f(counter: Counter) { let Counter { hits, .. } = counter; drop(hits); }",
                "a pattern takes it apart",
            ),
            (
                "fn f(counter: Counter, mut hits: Arc<Mutex<u64>>) { Counter { hits, .. } = counter; drop(hits); }",
                "a destructuring assignment takes it apart",
            ),
            (
                "macro_rules! hits { ($c:expr) => { $c.hits.lock() } }\nfn f(counter: &Counter) { drop(hits!(counter)); }",
                "it is used in a macro",
            ),
            (
                "fn f() -> Counter { let hits = Arc::new(Mutex::new(0)); Counter { hits: hits.clone(), ..Counter::default() } }\nfn g(all: &[Arc<Mutex<u64>>]) -> Counter { Counter { hits: all[0].clone(), name: String::new() } }",
                "a struct literal sets it to a value fix can't follow",
            ),
            (
                "type Alias = Counter;\nfn f() -> Alias { Alias { hits: Arc::new(Mutex::new(0)), name: String::new() } }",
                "a struct literal fix can't trace to its struct sets it",
            ),
            (
                "impl Counter {\n    #[inline_always::always]\n    fn f(&self) { *self.hits.lock().unwrap() += 1; }\n}",
                "an attribute macro reads code that names it",
            ),
        ] {
            refused(body, Reason::Unfollowed(reason));
        }
    }

    #[test]
    fn a_guard_naming_its_receiver_again_is_refused() {
        refused(
            "impl Counter { fn f(&self) { self.hits.lock().unwrap().clone_from(&self); } }",
            Reason::Unfollowed("a statement holding its guard names it again"),
        );
    }

    #[test]
    fn locks_that_might_run_inside_an_actor_callback_are_refused() {
        for (body, reason) in [
            (
                "impl Counter { fn f(&self) { let bump = || *self.hits.lock().unwrap() += 1; bump(); } }",
                Reason::Nested(
                    "it is locked in a closure or an async block, which might run inside an actor's callback",
                ),
            ),
            (
                "impl Counter { async fn f(&self) { *self.hits.lock().unwrap() += 1; } }",
                Reason::Poll,
            ),
            (
                "impl Counter { fn hit(&self) { *self.hits.lock().unwrap() += 1; } }\nimpl Drop for Counter { fn drop(&mut self) { self.hit(); } }",
                Reason::Nested(
                    "it is locked through a trait method, which might run inside an actor's callback",
                ),
            ),
            (
                "impl Counter { fn hit(&self) { *self.hits.lock().unwrap() += 1; } }\nfn later(counter: Counter) -> impl Fn() { move || counter.hit() }",
                Reason::Nested(
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback",
                ),
            ),
            (
                "impl Counter { fn hit(&self) { *self.hits.lock().unwrap() += 1; } }\nfn all(counters: &[Counter]) { counters.iter().for_each(Counter::hit); }",
                Reason::Nested(
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback",
                ),
            ),
            (
                "impl Counter { fn hit(&self) { *self.hits.lock().unwrap() += 1; } }\nfn f(actor: &adamantium::Actor<Counter>) { actor.with(|counter| counter.hit()); }",
                Reason::Nested("an actor callback in its package calls code that might lock it"),
            ),
        ] {
            refused(body, reason);
        }
    }

    #[test]
    fn existing_callbacks_matter_only_to_fields_something_locks() {
        let (fixed, refusals) = fix(
            &format!(
                "{COUNTER}fn make(actor: &adamantium::Actor<u8>) -> Counter {{\n    actor.with(|n| n.count_ones());\n    Counter {{ hits: Arc::new(Mutex::new(0)), name: String::new() }}\n}}\n"
            ),
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("hits: ::adamantium::Actor::new(0)"),
            "{fixed}"
        );
    }

    #[test]
    fn state_that_might_be_unsized_is_left_alone() {
        for source in [
            "use std::sync::{Arc, Mutex};\nstruct Shared<T: ?Sized> { state: Arc<Mutex<T>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Shared<T> where T: ?Sized { state: Arc<Mutex<T>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Shared<T: Iterator> { state: Arc<Mutex<T::Item>> }\n",
            "use std::sync::{Arc, Mutex};\nstruct Shared<T: Iterator> { state: Arc<Mutex<<T as Iterator>::Item>> }\n",
        ] {
            let (fixed, refusals) = fix(
                source,
                &[
                    "Mutex",
                    "Mutex<T>",
                    "Mutex<T::Item>",
                    "Mutex<<T as Iterator>::Item>",
                ],
            );
            assert_eq!(fixed, source);
            assert!(
                refusals.contains(&(
                    "Shared.state".to_owned(),
                    Reason::Unfollowed("its state might be unsized, which an actor can't hold")
                )),
                "{refusals:?}\n{source}"
            );
        }
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nstruct Shared<T> { state: Arc<Mutex<T>> }\n",
            &["Mutex", "Mutex<T>"],
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("state: ::adamantium::Actor<T>"), "{fixed}");
    }

    #[test]
    fn a_public_struct_keeps_its_private_fields_rewritable() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\npub struct Counter {\n    hits: Arc<Mutex<u64>>,\n}\nimpl Counter {\n    fn hit(&self) {\n        *self.hits.lock().unwrap() += 1;\n    }\n}\n",
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("self.hits.with(|hits| *hits += 1);"),
            "{fixed}"
        );
    }

    #[test]
    fn a_statement_calling_a_locking_method_keeps_its_own_rewrite() {
        let source = format!(
            "{COUNTER}impl Counter {{ fn hit(&self) {{ *self.hits.lock().unwrap() += 1; }} }}\nfn f() {{ let all = Arc::new(Mutex::new(Counter::default())); all.lock().unwrap().hit(); }}\n"
        );
        let (fixed, refusals) = fix(&source, &["Mutex", "Mutex<u64>", "Mutex<Counter>"]);
        assert_eq!(
            refusals,
            [(
                "Counter.hits".to_owned(),
                Reason::Nested(
                    "a function that locks it is called in a statement that becomes an actor's callback"
                )
            )]
        );
        assert!(
            fixed.contains("all.with(|all| { (*all).hit(); });"),
            "{fixed}"
        );
        assert!(fixed.contains("hits: Arc<Mutex<u64>>"), "{fixed}");
    }

    #[test]
    fn a_getter_named_like_its_field_is_rewritten() {
        let (fixed, refusals) = fix(
            &format!(
                "{COUNTER}impl Counter {{\n    fn hits(&self) -> u64 {{\n        *self.hits.lock().unwrap()\n    }}\n}}\n"
            ),
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert!(fixed.contains("self.hits.read(|hits| *hits)"), "{fixed}");
    }

    #[test]
    fn a_function_imported_under_another_name_still_reaches_the_field() {
        let source = format!(
            "{COUNTER}fn hit(counter: &Counter) {{ *counter.hits.lock().unwrap() += 1; }}\nuse crate::hit as bump;\nstruct State {{ counter: Counter }}\nimpl State {{ fn go(&self) {{ bump(&self.counter); }} }}\nfn f() {{ let outer = Arc::new(Mutex::new(State {{ counter: Counter::default() }})); outer.lock().unwrap().go(); }}\n"
        );
        let (fixed, refusals) = fix(&source, &["Mutex", "Mutex<u64>", "Mutex<State>"]);
        assert_eq!(
            refusals,
            [(
                "Counter.hits".to_owned(),
                Reason::Nested(
                    "a function that locks it is called in a statement that becomes an actor's callback"
                )
            )]
        );
        assert!(fixed.contains("hits: Arc<Mutex<u64>>"), "{fixed}");
    }

    #[test]
    fn traced_parameters_locals_and_impls_are_rewritten() {
        let (fixed, refusals) = fix(
            &format!(
                "{COUNTER}fn hit(counter: &mut Counter) {{\n    *counter.hits.lock().unwrap() += 1;\n}}\nfn fresh() -> u64 {{\n    let counter = Counter {{ hits: Arc::new(Mutex::new(1)), name: String::new() }};\n    let typed: &Counter = &counter;\n    let n = *typed.hits.lock().unwrap();\n    n\n}}\nimpl Clone for Box<u8> {{}}\ntrait Total {{ fn total(&self) -> u64; }}\nfn by_closure(all: &[Counter]) -> u64 {{\n    all.iter().map(|c: &Counter| c.name.len() as u64).sum()\n}}\n"
            ),
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("counter.hits.with(|hits| *hits += 1);"),
            "{fixed}"
        );
        assert!(
            fixed.contains("let n = typed.hits.read(|hits| *hits);"),
            "{fixed}"
        );
    }

    #[test]
    fn another_file_naming_the_field_refuses_it() {
        let source = format!("{COUNTER}fn f(c: &Counter) {{ *c.hits.lock().unwrap() += 1; }}\n");
        let spans = mutex_spans(&source, MUTEXES);
        for (other, refused) in [
            ("fn g(x: (u8,)) -> u8 { let hits = x.0; hits }\n", false),
            ("fn g(x: &super::Thing) -> u8 { x.hits }\n", true),
            ("macro_rules! m { () => { hits } }\n", true),
        ] {
            let mut krate = Crate::default();
            krate.add(Path::new("lib.rs"), &source);
            krate.add(Path::new("other.rs"), other);
            let settled = settle(
                vec![plan(Path::new("lib.rs"), &source, &spans, &krate)],
                &krate,
            );
            let refusals: Vec<_> = settled
                .iter()
                .flat_map(|(_, outcome)| &outcome.refusals)
                .map(|refusal| refusal.reason)
                .collect();
            assert_eq!(!refusals.is_empty(), refused, "{other}: {refusals:?}");
        }
    }

    #[test]
    fn public_functions_matter_only_in_a_library() {
        let source = format!(
            "{COUNTER}pub fn hit() {{\n    let c = Counter {{ hits: Arc::new(Mutex::new(0)), name: String::new() }};\n    *c.hits.lock().unwrap() += 1;\n}}\n"
        );
        let spans = mutex_spans(&source, MUTEXES);
        for (file, refused) in [("src/main.rs", false), ("src/lib.rs", true)] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
            fs::write(dir.path().join(file), &source).unwrap();
            let path = dir.path().join(file).canonicalize().unwrap();
            let krate = Crate::scan(dir.path()).unwrap();
            let settled = settle(vec![plan(&path, &source, &spans, &krate)], &krate);
            let reasons: Vec<_> = settled
                .iter()
                .flat_map(|(_, outcome)| &outcome.refusals)
                .map(|refusal| refusal.reason)
                .collect();
            assert_eq!(
                reasons.contains(&Reason::Nested(
                    "it is locked through a public function, which another crate might call inside an actor's callback"
                )),
                refused,
                "{file}: {reasons:?}"
            );
        }
    }
}
