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
