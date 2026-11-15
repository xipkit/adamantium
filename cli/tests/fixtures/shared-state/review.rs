//! Shared state that `cargo adamantium fix` leaves for review. Each function
//! holds one binding that a mechanical rewrite can't follow everywhere it is
//! used; the test checks that the file is unchanged and that each is reported
//! with its section of the migration guide.

use std::sync::{Arc, Mutex, RwLock};

macro_rules! touch {
    ($state:expr) => {
        drop($state)
    };
}

pub fn held() -> u32 {
    let held = Arc::new(Mutex::new(1u32));
    let mut guard = held.lock().unwrap();
    *guard += 1;
    *guard
}

pub async fn awaited() -> Vec<u32> {
    let awaited = Arc::new(Mutex::new(Vec::new()));
    awaited.lock().unwrap().push(next().await);
    let values = awaited.lock().unwrap().clone();
    values
}

async fn next() -> u32 {
    1
}

pub fn transfer() -> u64 {
    let checking = Arc::new(Mutex::new(100u64));
    let savings = Arc::new(Mutex::new(0u64));
    *savings.lock().unwrap() += 30;
    *checking.lock().unwrap() -= *savings.lock().unwrap();
    let total = *checking.lock().unwrap() + 30;
    total
}

/// `other` might call an actor, which panics inside an actor's callback.
pub fn calls() -> u32 {
    let calls = Arc::new(Mutex::new(1u32));
    *calls.lock().unwrap() += other();
    let value = *calls.lock().unwrap();
    value
}

fn other() -> u32 {
    1
}

pub fn escapes() -> usize {
    let escapes = Arc::new(Mutex::new(vec![1]));
    let shared = Arc::clone(&escapes);
    escapes.lock().unwrap().push(2);
    Arc::strong_count(&shared)
}

pub fn macro_use() -> u8 {
    let macro_use = Arc::new(Mutex::new(1u8));
    touch!(macro_use.clone());
    let value = *macro_use.lock().unwrap();
    value
}

/// Locked in a closure, which might run inside an actor's callback.
fn deferred() -> u32 {
    let deferred = Arc::new(Mutex::new(1u32));
    let bump = || *deferred.lock().unwrap() += 1;
    bump();
    let value = *deferred.lock().unwrap();
    value
}

/// Locked in async code, which fix doesn't follow yet.
pub async fn pending() -> u32 {
    let pending = Arc::new(Mutex::new(deferred()));
    *pending.lock().unwrap() += 1;
    let value = *pending.lock().unwrap();
    value
}

/// Another crate might call this inside an actor's callback.
pub fn public() -> u32 {
    let public = Arc::new(Mutex::new(1u32));
    *public.lock().unwrap() += 1;
    let value = *public.lock().unwrap();
    value
}

pub fn routes() -> usize {
    let routes = Arc::new(RwLock::new(vec!["/"]));
    routes.write().unwrap().push("/about");
    let count = routes.read().unwrap().len();
    count
}
