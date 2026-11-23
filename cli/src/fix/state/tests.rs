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
        let source = format!("use std::sync::{{Arc, Mutex}};\n{body}\nfn consume<T>(_: T) {{}}\n");
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
                && *reason == Reason::Unfollowed("a statement holding its guard names it again")),
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
    const COUNT: &str = "fn count() { let m = Arc::new(Mutex::new(0)); *m.lock().unwrap() += 1; }";
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
        Reason::Unfollowed("a statement holding its guard may move a variable on only one path"),
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
