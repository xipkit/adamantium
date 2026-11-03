#![allow(dead_code)]
use std::{cell::RefCell, rc::Rc, sync::Mutex, time::Duration};

fn identity() -> Rc<RefCell<i32>> {
    Rc::new(RefCell::new(1))
}

fn unwraps(x: Option<i32>, r: Result<i32, String>) -> i32 {
    x.unwrap() + r.expect("boom")
}

fn index(v: &[i32]) -> i32 {
    v[0]
}

fn slice(s: &str) -> &str {
    &s[0..1]
}

fn panics() {
    panic!("no");
}

fn narrow(x: i64) -> i32 {
    x as i32
}

fn forget(s: String) {
    std::mem::forget(s);
}

fn threads() {
    std::thread::spawn(|| {});
    std::thread::sleep(Duration::from_millis(1));
}

fn debug() {
    dbg!(1);
}

fn quit() {
    std::process::exit(1);
}

fn raw() -> i32 {
    let x = 1;
    unsafe { *std::ptr::addr_of!(x) }
}

async fn held(m: &Mutex<i32>) {
    let guard = m.lock().unwrap_or_else(|e| e.into_inner());
    tokio::time::sleep(Duration::from_millis(1)).await;
    drop(guard);
}

fn spawns() {
    tokio::spawn(async {});
    let _set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
}

async fn not_send(r: Rc<i32>) -> i32 {
    tokio::time::sleep(Duration::from_millis(1)).await;
    *r
}
