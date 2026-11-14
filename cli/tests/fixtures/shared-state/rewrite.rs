//! Every form of shared state that `cargo adamantium fix` rewrites. The test
//! applies the fix, compares the result with `rewrite.fixed.rs`, checks it and
//! runs these tests against it.

use std::sync::{Arc, Mutex};
use std::{
    sync::{self, Arc as Shared, Mutex as Lock},
    time::Duration,
};

pub fn counter() -> u64 {
    let hits = Arc::new(Mutex::new(0u64));
    let handle = Arc::clone(&hits);
    *handle.lock().unwrap() += 1;
    assert_eq!(*hits.lock().unwrap(), 1);
    let total = *hits.lock().unwrap() + 1;
    total
}

pub fn settings() -> (Vec<&'static str>, usize) {
    let settings: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::default());
    let writer = settings.clone();
    writer.lock().unwrap().push("verbose");
    let current = settings.lock().expect("settings lock").clone();
    let count = settings.lock().unwrap().len();
    (current, count)
}

pub fn aliased() -> u32 {
    let state = Shared::new(Lock::new(2u32));
    let full = std::sync::Arc::new(std::sync::Mutex::new(3u32));
    let module = sync::Arc::new(sync::Mutex::new(4u32));
    let copy: Shared<Lock<u32>> = Shared::clone(&state);
    *copy.lock().unwrap() *= 5;
    let product = *state.lock().unwrap() * 3;
    let sum = product + *full.lock().unwrap();
    sum * *module.lock().unwrap()
}

#[derive(Debug, Default)]
pub struct Cart {
    pub items: Vec<&'static str>,
    pub total: u64,
}

pub fn branches(prices: &[u64]) -> u64 {
    let cart = Arc::new(Mutex::new(Cart::default()));
    for price in prices {
        cart.lock().unwrap().total += price;
    }
    let add = |item: &'static str| cart.lock().unwrap().items.push(item);
    add("pen");
    if cart.lock().unwrap().total > 10 {
        cart.lock().unwrap().items.push("discount");
    }
    while cart.lock().unwrap().items.len() < 3 {
        cart.lock().unwrap().items.push("filler")
    }
    let kind = match prices.len() {
        0 => cart.lock().unwrap().items.len(),
        n if n > cart.lock().unwrap().items.len() => n,
        _ => 0,
    };
    let empty = prices.is_empty() || cart.lock().unwrap().items.is_empty();
    if empty {
        return cart.lock().unwrap().total;
    }
    let extra = if kind > 0 {
        cart.lock().unwrap().items.len()
    } else {
        0
    };
    cart.lock().unwrap().total + kind as u64 + extra as u64
}

pub async fn deferred() -> Vec<u32> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let task = {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(1);
        }
    };
    task.await;
    (2..4).for_each(|n| log.lock().unwrap().push(n));
    let shadow = log.lock().unwrap().len();
    {
        let log = shadow;
        assert_eq!(log, 3);
    }
    let spare = log.clone();
    std::mem::drop(spare);
    let entries = log.lock().unwrap().clone();
    entries
}

pub fn timeout() -> Duration {
    Duration::from_millis(1)
}

/// Implemented for the state and for a reference to it, so a method call
/// resolves to the state's only if lookup starts where the guard's did.
pub trait Value {
    fn value(&self) -> u32;
}

impl Value for u32 {
    fn value(&self) -> u32 {
        *self
    }
}

impl Value for &mut u32 {
    fn value(&self) -> u32 {
        0
    }
}

pub fn lookup() -> u32 {
    let state = Arc::new(Mutex::new(7u32));
    let value = state.lock().unwrap().value();
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken() -> Option<String> {
        let name = Arc::new(Mutex::new(String::from("adamantium")));
        let label = format!("{:?}", name.lock().unwrap().len());
        assert_eq!(label, "10");
        Some(Arc::into_inner(name).unwrap().into_inner().unwrap())
    }

    fn unwrapped() -> Vec<u8> {
        let log = Arc::new(Mutex::new(vec![1u8]));
        let spare = Arc::clone(&log);
        drop(spare);
        Arc::try_unwrap(log).expect("one owner").into_inner().unwrap()
    }

    #[test]
    fn rewritten_state_behaves_the_same() {
        assert_eq!(counter(), 2);
        assert_eq!(settings(), (vec!["verbose"], 1));
        assert_eq!(aliased(), 132);
        assert_eq!(branches(&[4, 8]), 12);
        assert_eq!(branches(&[]), 0);
        assert_eq!(taken().as_deref(), Some("adamantium"));
        assert_eq!(unwrapped(), [1]);
        assert_eq!(timeout(), Duration::from_millis(1));
        assert_eq!(lookup(), 7);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(deferred()), [1, 2, 3]);
    }
}
