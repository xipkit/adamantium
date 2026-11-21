use std::collections::hash_map::Entry;

use syn::{Block, Expr, ImplItem, Item, Pat, PathArguments, ReturnType, Stmt, Type, visit::Visit};

use super::{
    Edit, Group, ItemScope, LockKind, Reason, Resolved, UNLOCATED, Walker, bare,
    fields::may_be_unsized, item_attributes,
};
use crate::fix::scan::macro_attribute;

pub(super) struct Callee {
    pub(super) name: String,
    owner: Option<String>,
    receiver: bool,
    params: Vec<Option<(usize, bool)>>,
    returns: Option<usize>,
    pub(super) calls: usize,
}

impl Callee {
    pub(super) fn groups(&self) -> impl Iterator<Item = usize> + '_ {
        self.params
            .iter()
            .filter_map(|param| param.map(|(group, _)| group))
            .chain(self.returns)
    }
}

pub(super) struct Frame<'a> {
    pub(super) group: usize,
    tail: Option<&'a Expr>,
}

const ARGUMENT: Reason = Reason::Unfollowed("a call passes it a value fix can't follow");

const RETURNED: Reason = Reason::Unfollowed("its function returns a value fix can't follow");

impl<'a> Walker<'a> {
    pub(super) fn callee_candidates(&mut self, items: &'a [Item]) {
        for item in items {
            if macro_attribute(item_attributes(item)) {
                continue;
            }
            match item {
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        self.items.push(ItemScope::new(items, true));
                        self.callee_candidates(items);
                        self.items.pop();
                    }
                }
                Item::Fn(function) => self.generic(&function.sig.generics, |walker| {
                    walker.callee_candidate(&function.vis, &function.attrs, &function.sig, None);
                }),
                Item::Impl(item) if item.trait_.is_none() => {
                    self.generic(&item.generics, |walker| {
                        let Some(owner) = walker.owner(item) else {
                            return;
                        };
                        for member in &item.items {
                            if let ImplItem::Fn(function) = member {
                                walker.generic(&function.sig.generics, |walker| {
                                    walker.callee_candidate(
                                        &function.vis,
                                        &function.attrs,
                                        &function.sig,
                                        Some((&owner, &item.generics)),
                                    );
                                });
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    }

    fn callee_candidate(
        &mut self,
        vis: &syn::Visibility,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        owner: Option<(&String, &syn::Generics)>,
    ) {
        if !matches!(vis, syn::Visibility::Inherited)
            || macro_attribute(attrs)
            || sig.asyncness.is_some()
            || sig.abi.is_some()
            || sig.variadic.is_some()
        {
            return;
        }
        let generics = [Some(&sig.generics), owner.map(|(_, generics)| generics)];
        let sized = |state: &Type| {
            !generics
                .iter()
                .flatten()
                .any(|generics| may_be_unsized(state, generics))
        };
        let mut params = Vec::new();
        for input in &sig.inputs {
            let syn::FnArg::Typed(typed) = input else {
                continue;
            };
            let param = match &*typed.pat {
                Pat::Ident(ident) if ident.by_ref.is_none() && ident.subpat.is_none() => self
                    .parameter_type(&typed.ty, &sized)
                    .map(|(edit, kind, lock_ref)| {
                        let group = self.groups.len();
                        self.groups.push(Group::new(
                            bare(&ident.ident),
                            self.text.line(ident.ident.span()),
                            kind,
                            vec![edit],
                        ));
                        (group, lock_ref)
                    }),
                _ => None,
            };
            params.push(param);
        }
        let name = bare(&sig.ident);
        let returns = match &sig.output {
            ReturnType::Type(_, ty) => self
                .arc_state(ty)
                .filter(|(_, state)| sized(state))
                .and_then(|(kind, state)| {
                    Some((kind, self.edit(self.text.of(ty)?, self.actor_of(state)?)))
                })
                .map(|(kind, edit)| {
                    let group = self.groups.len();
                    self.groups.push(Group::new(
                        format!("{name}()"),
                        self.text.line(sig.ident.span()),
                        kind,
                        vec![edit],
                    ));
                    group
                }),
            ReturnType::Default => None,
        };
        if returns.is_none() && params.iter().all(Option::is_none) {
            return;
        }
        let index = self.callees.len();
        match self.callee_names.entry(name.clone()) {
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
            Entry::Vacant(entry) => {
                entry.insert(Some(index));
            }
        }
        if let Some(range) = self.text.of(&sig.ident) {
            self.signatures.insert(range.start, index);
        }
        self.callees.push(Callee {
            name,
            owner: owner.map(|(owner, _)| owner.clone()),
            receiver: sig.receiver().is_some(),
            params,
            returns,
            calls: 0,
        });
    }

    fn parameter_type(
        &self,
        ty: &Type,
        sized: &impl Fn(&Type) -> bool,
    ) -> Option<(Edit, LockKind, bool)> {
        let (held, (kind, state), lock_ref) = match ty {
            Type::Reference(reference) if reference.mutability.is_none() => {
                match self.lock_state(&reference.elem) {
                    Some(state) => (&*reference.elem, state, true),
                    None => (&*reference.elem, self.arc_state(&reference.elem)?, false),
                }
            }
            ty => (ty, self.arc_state(ty)?, false),
        };
        if !sized(state) {
            return None;
        }
        Some((
            self.edit(self.text.of(held)?, self.actor_of(state)?),
            kind,
            lock_ref,
        ))
    }

    pub(super) fn owner(&self, item: &syn::ItemImpl) -> Option<String> {
        let Type::Path(path) = &*item.self_ty else {
            return None;
        };
        if path.qself.is_some() || path.path.leading_colon.is_some() {
            return None;
        }
        let [segment] = path.path.segments.iter().collect::<Vec<_>>()[..] else {
            return None;
        };
        let name = bare(&segment.ident);
        (self.declared.get(&name).copied() == Some(1)
            && matches!(self.resolve_name(&name), Resolved::Item))
        .then_some(name)
    }

    pub(super) fn signature(&self, sig: &syn::Signature) -> Option<usize> {
        let start = self.text.of(&sig.ident)?.start;
        self.signatures.get(&start).copied()
    }

    pub(super) fn parameter(&mut self, callee: usize, position: usize, pat: &Pat) {
        let Some((group, lock_ref)) = self
            .callees
            .get(callee)
            .and_then(|callee| callee.params.get(position).copied().flatten())
        else {
            return;
        };
        let Pat::Ident(ident) = pat else {
            return;
        };
        if let Some(var) = self.lookup(&bare(&ident.ident))
            && let Some(entry) = self.vars.get_mut(var)
        {
            entry.group = Some(group);
            entry.lock_ref = lock_ref;
            entry.spelling = ident.ident.to_string();
        }
    }

    pub(super) fn frame(&mut self, callee: usize, body: &'a Block) -> Option<Frame<'a>> {
        let group = self.callees.get(callee)?.returns?;
        let tail = match body.stmts.last() {
            Some(Stmt::Expr(expr, None)) => Some(expr),
            Some(Stmt::Macro(mac)) if mac.semi_token.is_none() => {
                if let Some(state) = self.groups.get_mut(group) {
                    state.refuse(RETURNED);
                }
                None
            }
            _ => None,
        };
        Some(Frame { group, tail })
    }

    pub(super) fn tail(&mut self, expr: &Expr) -> Option<usize> {
        let frame = self.returns.last_mut()?.as_mut()?;
        if !frame.tail.is_some_and(|tail| super::is(tail, expr)) {
            return None;
        }
        frame.tail = None;
        Some(frame.group)
    }

    pub(super) fn followed_return(&mut self, expr: &'a Expr) -> bool {
        let Expr::Return(syn::ExprReturn {
            expr: Some(value), ..
        }) = expr
        else {
            return false;
        };
        let Some(Some(frame)) = self.returns.last() else {
            return false;
        };
        let group = frame.group;
        self.returned(group, value);
        true
    }

    pub(super) fn returned(&mut self, group: usize, expr: &'a Expr) {
        if let Some((_, constructor)) = self.constructor(expr)
            && let Some(range) = self.text.of(expr)
        {
            self.visit_expr(expr);
            let edit = self.edit(range, constructor);
            if let Some(group) = self.groups.get_mut(group) {
                group.edits.push(edit);
            }
            return;
        }
        if let Some((source, replacement)) = self.clone_of(expr) {
            self.flows(group, source, expr, replacement);
            return;
        }
        if let Some(returned) = self.returned_by(expr) {
            self.links.push((group, returned));
            return;
        }
        if let Some(state) = self.groups.get_mut(group) {
            state.refuse(RETURNED);
        }
        self.visit_expr(expr);
    }

    pub(super) fn flows(
        &mut self,
        group: usize,
        source: usize,
        expr: &Expr,
        replacement: Option<String>,
    ) {
        if let Some(var) = self.vars.get_mut(source) {
            var.used = true;
        }
        if let Some(held) = self.group_of(source) {
            self.links.push((group, held));
        }
        let Some(replacement) = replacement else {
            return;
        };
        let edit = self
            .text
            .of(expr)
            .map(|range| self.edit(range, replacement));
        if let Some(state) = self.groups.get_mut(group) {
            match edit {
                Some(edit) => state.edits.push(edit),
                None => state.refuse(UNLOCATED),
            }
        }
    }

    fn callee(&self, expr: &Expr) -> Option<(usize, usize)> {
        let named = |name: &syn::Ident| {
            let index = self.callee_names.get(&bare(name)).copied().flatten()?;
            Some((index, self.callees.get(index)?))
        };
        let owner = self.owners.last().cloned().flatten();
        match expr {
            Expr::Call(call) => {
                let Expr::Path(func) = &*call.func else {
                    return None;
                };
                if func.qself.is_some() || func.path.leading_colon.is_some() {
                    return None;
                }
                match func.path.segments.iter().collect::<Vec<_>>()[..] {
                    [function] => {
                        let (index, callee) = named(&function.ident)?;
                        let name = bare(&function.ident);
                        (callee.owner.is_none()
                            && self.lookup(&name).is_none()
                            && matches!(self.resolve_name(&name), Resolved::Item))
                        .then_some((index, 0))
                    }
                    [ty, function] => {
                        let (index, callee) = named(&function.ident)?;
                        let expected = callee.owner.as_ref()?;
                        let means = if ty.ident == "Self" {
                            owner.as_ref() == Some(expected)
                        } else {
                            bare(&ty.ident) == *expected
                                && matches!(ty.arguments, PathArguments::None)
                                && matches!(self.resolve_name(expected), Resolved::Item)
                        };
                        means.then_some((index, usize::from(callee.receiver)))
                    }
                    _ => None,
                }
            }
            Expr::MethodCall(call) => {
                let (index, callee) = named(&call.method)?;
                let Expr::Path(receiver) = &*call.receiver else {
                    return None;
                };
                (callee.receiver
                    && callee.owner.is_some()
                    && callee.owner == owner
                    && receiver.qself.is_none()
                    && receiver.attrs.is_empty()
                    && receiver.path.is_ident("self"))
                .then_some((index, 0))
            }
            _ => None,
        }
    }

    fn arguments(&mut self, callee: usize, offset: usize, expr: &'a Expr) {
        let args = match expr {
            Expr::Call(call) => &call.args,
            Expr::MethodCall(call) => &call.args,
            _ => return,
        };
        if let Some(callee) = self.callees.get_mut(callee) {
            callee.calls += 1;
        }
        for (position, arg) in args.iter().enumerate() {
            let param = position.checked_sub(offset).and_then(|position| {
                self.callees
                    .get(callee)?
                    .params
                    .get(position)
                    .copied()
                    .flatten()
            });
            match param {
                Some((group, _)) => self.argument(group, arg),
                None => self.visit_expr(arg),
            }
        }
    }

    fn argument(&mut self, group: usize, arg: &'a Expr) {
        let source = match arg {
            Expr::Reference(reference)
                if reference.mutability.is_none() && reference.attrs.is_empty() =>
            {
                self.tracked(&reference.expr).map(|var| (var, None))
            }
            _ => self.clone_of(arg),
        };
        match source {
            Some((var, replacement)) => self.flows(group, var, arg, replacement),
            None => {
                if let Some(state) = self.groups.get_mut(group) {
                    state.refuse(ARGUMENT);
                }
                self.visit_expr(arg);
            }
        }
    }

    pub(super) fn followed_call(&mut self, expr: &'a Expr) -> bool {
        let Some((callee, offset)) = self.callee(expr) else {
            return false;
        };
        if let Some(group) = self.callees.get(callee).and_then(|callee| callee.returns)
            && let Some(state) = self.groups.get_mut(group)
        {
            state.refuse(Reason::Unfollowed(
                "a call uses its value where fix can't follow it",
            ));
        }
        self.arguments(callee, offset, expr);
        true
    }

    pub(super) fn returned_by(&mut self, expr: &'a Expr) -> Option<usize> {
        let (callee, offset) = self.callee(expr)?;
        let group = self.callees.get(callee)?.returns?;
        self.stack.push(super::Node::Expr(expr));
        self.arguments(callee, offset, expr);
        self.stack.pop();
        Some(group)
    }

    pub(super) fn mention_callee(&mut self, name: &str, reason: Reason) {
        let groups: Vec<usize> = self
            .callees
            .iter()
            .filter(|callee| callee.name == name)
            .flat_map(Callee::groups)
            .collect();
        for group in groups {
            if let Some(group) = self.groups.get_mut(group) {
                group.refuse(reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::{
        Reason, plan, settle,
        tests::{fix, mutex_spans},
    };
    use crate::fix::scan::Crate;

    const MUTEXES: &[&str] = &["Mutex", "Mutex<u64>"];

    const BUMP: &str = "fn bump(hits: &Arc<Mutex<u64>>) {\n    *hits.lock().unwrap() += 1;\n}\n";

    fn refused(body: &str, name: &str, reason: Reason) {
        let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\n");
        let (fixed, refusals) = fix(&source, MUTEXES);
        assert_eq!(fixed, source);
        assert!(
            refusals.contains(&(name.to_owned(), reason)),
            "{refusals:?}\n{source}"
        );
    }

    #[test]
    fn calls_fix_cannot_follow_refuse_the_parameter() {
        for body in [
            "fn f() { bump(&Arc::new(Mutex::new(1))); }",
            "fn f(all: &[Arc<Mutex<u64>>]) { bump(&all[0]); }",
            "fn f() { let hits = Arc::new(Mutex::new(1)); bump(&&hits); }",
        ] {
            refused(
                &format!("{BUMP}{body}"),
                "hits",
                Reason::Unfollowed("a call passes it a value fix can't follow"),
            );
        }
    }

    #[test]
    fn a_function_named_where_fix_cannot_follow_is_refused() {
        for body in [
            "fn f() { let call: fn(&Arc<Mutex<u64>>) = bump; drop(call); }",
            "fn f() { let hits = Arc::new(Mutex::new(1)); [&hits].into_iter().for_each(bump); }",
            "mod inner { fn bump() {} }",
            "mod inner { fn f(hits: &std::sync::Arc<std::sync::Mutex<u64>>) { super::bump(hits); } }",
            "use bump as hit;",
            "macro_rules! hit { ($h:expr) => { bump($h) } }",
            "struct S { bump: u8 }",
            "fn f(doc: fn(&Arc<Mutex<u64>>)) { let all = [doc, bump]; drop(all); }",
        ] {
            refused(
                &format!("{BUMP}{body}"),
                "hits",
                Reason::Unfollowed("its function is named somewhere fix doesn't follow as a call"),
            );
        }
    }

    #[test]
    fn a_function_another_file_names_is_refused() {
        let source = format!(
            "use std::sync::{{Arc, Mutex}};\n{BUMP}fn f() {{ let m = Arc::new(Mutex::new(1)); bump(&m); }}\n"
        );
        let spans = mutex_spans(&source, MUTEXES);
        for (other, refused) in [
            ("fn g() -> u8 { 1 }\n", false),
            ("fn g() { super::bump(&todo!()); }\n", true),
            ("#[serde(default = \"bump\")]\nstruct S;\n", true),
            ("/// Calls bump.\nfn g() {}\n", false),
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
                .map(|refusal| (refusal.name.as_str(), refusal.reason))
                .collect();
            assert_eq!(!refusals.is_empty(), refused, "{other}: {refusals:?}");
        }
    }

    #[test]
    fn functions_fix_does_not_rewrite_leave_their_arguments_alone() {
        for function in [
            "pub fn take(hits: &Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; }",
            "pub(crate) fn take(hits: &Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; }",
            "async fn take(hits: &Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; }",
            "fn take(hits: &mut Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; }",
            "fn take<T: ?Sized>(hits: &Mutex<T>) { drop(hits.lock().unwrap()); }",
            "fn take(Wrapper(hits): Wrapper) { drop(hits); }\nstruct Wrapper(u8);",
            "#[some::attribute]\nfn take(hits: &Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; }",
            "struct S;\ntrait T { fn take(hits: &Arc<Mutex<u64>>); }\nimpl T for S { fn take(hits: &Arc<Mutex<u64>>) { *hits.lock().unwrap() += 1; } }",
        ] {
            let call = if function.contains("trait T") {
                "S::take"
            } else {
                "take"
            };
            refused(
                &format!("{function}\nfn f() {{ let m = Arc::new(Mutex::new(1)); {call}(&m); }}"),
                "m",
                Reason::Unfollowed("it is passed, returned or stored where fix can't follow it"),
            );
        }
    }

    #[test]
    fn methods_are_followed_only_through_self_and_their_type() {
        for call in ["s.take(&m)", "Other::take(&m)", "<S>::take(&m)"] {
            refused(
                &format!(
                    "{BUMP}struct S;\nstruct Other;\nimpl S {{ fn take(&self, hits: &Arc<Mutex<u64>>) {{ bump(hits); }} }}\nfn f(s: &S) {{ let m = Arc::new(Mutex::new(1)); {call}; }}"
                ),
                "hits",
                Reason::Unfollowed("its function is named somewhere fix doesn't follow as a call"),
            );
        }
    }

    #[test]
    fn return_values_fix_cannot_follow_are_refused() {
        for (body, reason) in [
            (
                "fn make(all: &[Arc<Mutex<u64>>]) -> Arc<Mutex<u64>> { all[0].clone() }",
                "its function returns a value fix can't follow",
            ),
            (
                "fn make(flag: bool) -> Arc<Mutex<u64>> { let m = Arc::new(Mutex::new(1)); if flag { m } else { Arc::new(Mutex::new(2)) } }",
                "its function returns a value fix can't follow",
            ),
            (
                "macro_rules! shared { () => { Arc::new(Mutex::new(1)) } }\nfn make() -> Arc<Mutex<u64>> { shared! {} }",
                "its function returns a value fix can't follow",
            ),
            (
                "fn make() -> Arc<Mutex<u64>> { let m = Arc::new(Mutex::new(1)); let other = || { return 1; }; drop(other); { m } }",
                "its function returns a value fix can't follow",
            ),
            (
                "fn make() -> Arc<Mutex<u64>> { Arc::new(Mutex::new(1)) }\nfn f() { *make().lock().unwrap() += 1; }",
                "a call uses its value where fix can't follow it",
            ),
            (
                "fn make() -> Arc<Mutex<u64>> { Arc::new(Mutex::new(1)) }\nfn take(hits: &Arc<Mutex<u64>>) { drop(hits); }\nfn f() { take(&make()); }",
                "a call uses its value where fix can't follow it",
            ),
            (
                "fn make() -> Arc<Mutex<u64>> { Arc::new(Mutex::new(1)) }\nfn f() { let (m, n) = (make(), 1); drop((m, n)); }",
                "a call uses its value where fix can't follow it",
            ),
        ] {
            refused(body, "make()", Reason::Unfollowed(reason));
        }
    }

    const COUNT: &str = "fn count() -> u64 {\n    let hits = Arc::new(Mutex::new(0));\n    bump(&hits);\n    Arc::try_unwrap(hits).unwrap().into_inner().unwrap()\n}\n";

    #[test]
    fn a_state_locked_through_a_parameter_inside_a_callback_is_refused() {
        let source = format!(
            "use std::sync::{{Arc, Mutex}};\n{BUMP}{COUNT}fn f(total: &adamantium::Actor<u64>) {{ total.with(|total| *total += count()); }}\n"
        );
        let (fixed, refusals) = fix(&source, MUTEXES);
        assert_eq!(fixed, source);
        let callback =
            Reason::Nested("an actor callback in its package calls code that might lock it");
        let shared = Reason::Unfollowed("it holds the same state as a value fix leaves for review");
        assert!(
            refusals.iter().filter(|(name, _)| name == "hits").count() == 2
                && refusals
                    .iter()
                    .all(|(_, reason)| *reason == callback || *reason == shared)
                && refusals.iter().any(|(_, reason)| *reason == callback),
            "{refusals:?}"
        );
    }

    #[test]
    fn a_state_locked_through_a_parameter_in_a_new_callback_is_refused() {
        let source = format!(
            "use std::sync::{{Arc, Mutex}};\n{BUMP}{COUNT}struct S;\nimpl S {{ fn go(&self) -> u64 {{ count() }} }}\nfn f() {{ let s = Arc::new(Mutex::new(S)); s.lock().unwrap().go(); }}\n"
        );
        let (fixed, refusals) = fix(&source, &["Mutex", "Mutex<u64>", "Mutex<S>"]);
        let nested = Reason::Nested(
            "a function that locks it is called in a statement that becomes an actor's callback",
        );
        assert!(
            refusals
                .iter()
                .any(|(name, reason)| name == "hits" && *reason == nested),
            "{refusals:?}"
        );
        assert!(
            refusals.iter().all(|(name, _)| name == "hits"),
            "{refusals:?}"
        );
        assert!(fixed.contains("s.with(|s| { (*s).go(); });"), "{fixed}");
        assert!(fixed.contains(BUMP), "{fixed}");
        assert!(fixed.contains(COUNT), "{fixed}");
    }

    #[test]
    fn cloning_a_mutex_reference_is_refused() {
        refused(
            "fn f(hits: &Mutex<u64>) { let copy = hits.clone(); *copy.lock().unwrap() += 1; }\nfn g() { let m = Arc::new(Mutex::new(1)); f(&m); }",
            "hits",
            Reason::Unfollowed("it is passed, returned or stored where fix can't follow it"),
        );
    }

    #[test]
    fn a_closure_returns_its_own_values() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\nfn make() -> Arc<Mutex<u64>> {\n    let other = |n: u64| -> u64 { return n + 1; };\n    Arc::new(Mutex::new(other(1)))\n}\nfn f() -> u64 {\n    let m = make();\n    let n = *m.lock().unwrap();\n    n\n}\n",
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert!(
            fixed.contains("fn make() -> ::adamantium::Actor<u64> {"),
            "{fixed}"
        );
    }

    #[test]
    fn parameters_return_values_and_their_calls_become_actors() {
        let (fixed, refusals) = fix(
            "use std::sync::{Arc, Mutex};\n#[derive(Default)]\nstruct Counter {\n    hits: Arc<Mutex<u64>>,\n}\nimpl Counter {\n    fn hit(&self) {\n        self.bump(&self.hits);\n        Self::add(&self.hits, 2);\n    }\n    fn bump(&self, hits: &Arc<Mutex<u64>>) {\n        *hits.lock().unwrap() += 1;\n    }\n    fn add(hits: &Mutex<u64>, n: u64) {\n        *hits.lock().unwrap() += 2;\n        drop(n);\n    }\n}\nfn fresh() -> Arc<Mutex<u64>> {\n    Arc::new(Mutex::new(0))\n}\nfn share(hits: &Arc<Mutex<u64>>) -> Arc<Mutex<u64>> {\n    if true {\n        return Arc::clone(hits);\n    }\n    hits.clone()\n}\nfn take(hits: Arc<Mutex<u64>>) -> u64 {\n    let n = *hits.lock().unwrap();\n    n\n}\nfn main() {\n    let hits = fresh();\n    let other: Arc<Mutex<u64>> = share(&hits);\n    let counter = Counter { hits: share(&other) };\n    counter.hit();\n    let n = take(Arc::clone(&hits)) + take(hits);\n    drop(n);\n}\n",
            MUTEXES,
        );
        assert_eq!(refusals, []);
        assert_eq!(
            fixed,
            "#[derive(Default)]\nstruct Counter {\n    hits: ::adamantium::Actor<u64>,\n}\nimpl Counter {\n    fn hit(&self) {\n        self.bump(&self.hits);\n        Self::add(&self.hits, 2);\n    }\n    fn bump(&self, hits: &::adamantium::Actor<u64>) {\n        hits.with(|hits| *hits += 1);\n    }\n    fn add(hits: &::adamantium::Actor<u64>, n: u64) {\n        hits.with(|hits| *hits += 2);\n        drop(n);\n    }\n}\nfn fresh() -> ::adamantium::Actor<u64> {\n    ::adamantium::Actor::new(0)\n}\nfn share(hits: &::adamantium::Actor<u64>) -> ::adamantium::Actor<u64> {\n    if true {\n        return hits.clone();\n    }\n    hits.clone()\n}\nfn take(hits: ::adamantium::Actor<u64>) -> u64 {\n    let n = hits.read(|hits| *hits);\n    n\n}\nfn main() {\n    let hits = fresh();\n    let other: ::adamantium::Actor<u64> = share(&hits);\n    let counter = Counter { hits: share(&other) };\n    counter.hit();\n    let n = take(hits.clone()) + take(hits);\n    drop(n);\n}\n"
        );
    }
}
