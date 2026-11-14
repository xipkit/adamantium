//! Every form of shared state that `cargo adamantium fix` rewrites. The test
//! applies the fix, compares the result with `rewrite.fixed.rs`, checks it and
//! runs these tests against it.

use std::time::Duration;

pub fn counter() -> u64 {
    let hits = ::adamantium::Actor::new(0u64);
    let handle = hits.clone();
    handle.with(|handle| *handle += 1);
    hits.read(|hits| { assert_eq!(*hits, 1); });
    let total = hits.read(|hits| *hits) + 1;
    total
}

pub fn settings() -> (Vec<String>, usize) {
    let settings: ::adamantium::Actor<Vec<String>> = ::adamantium::Actor::default();
    let writer = settings.clone();
    writer.with(|writer| { writer.push("verbose".to_owned()); });
    let current = settings.snapshot();
    let count = settings.with(|settings| settings.len());
    (current, count)
}

pub fn aliased() -> u32 {
    let state = ::adamantium::Actor::new(2u32);
    let full = ::adamantium::Actor::new(3u32);
    let module = ::adamantium::Actor::new(4u32);
    let copy: ::adamantium::Actor<u32> = state.clone();
    copy.with(|copy| *copy *= 5);
    let product = state.read(|state| *state) * 3;
    let sum = product + full.read(|full| *full);
    sum * module.read(|module| *module)
}

#[derive(Debug, Default)]
pub struct Cart {
    pub items: Vec<String>,
    pub total: u64,
}

pub fn branches(prices: &[u64]) -> u64 {
    let cart = ::adamantium::Actor::new(Cart::default());
    for price in prices {
        cart.with(|cart| cart.total += price);
    }
    let add = |item: &str| cart.with(|cart| cart.items.push(item.to_owned()));
    add("pen");
    if cart.read(|cart| cart.total > 10) {
        cart.with(|cart| { cart.items.push("discount".to_owned()); });
    }
    while cart.with(|cart| cart.items.len() < 3) {
        cart.with(|cart| cart.items.push("filler".to_owned()))
    }
    let kind = match prices.len() {
        0 => cart.with(|cart| cart.items.len()),
        n if cart.with(|cart| n > cart.items.len()) => n,
        _ => 0,
    };
    let empty = prices.is_empty() || cart.with(|cart| cart.items.is_empty());
    if empty {
        return cart.read(|cart| cart.total);
    }
    let extra = if kind > 0 {
        cart.with(|cart| cart.items.len())
    } else {
        0
    };
    cart.read(|cart| cart.total) + kind as u64 + extra as u64
}

pub async fn deferred() -> Vec<u32> {
    let log = ::adamantium::Actor::new(Vec::new());
    let task = {
        let log = log.clone();
        async move {
            log.with(|log| { log.push(1); });
        }
    };
    task.await;
    (2..4).for_each(|n| log.with(|log| log.push(n)));
    let shadow = log.with(|log| log.len());
    {
        let log = shadow;
        assert_eq!(log, 3);
    }
    let spare = log.clone();
    std::mem::drop(spare);
    let entries = log.snapshot();
    entries
}

pub fn timeout() -> Duration {
    Duration::from_millis(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken() -> Option<String> {
        let name = ::adamantium::Actor::new(String::from("adamantium"));
        let label = name.with(|name| format!("{:?}", name.len()));
        assert_eq!(label, "10");
        Some(name.into_inner().ok().unwrap())
    }

    fn unwrapped() -> Vec<u8> {
        let log = ::adamantium::Actor::new(vec![1u8]);
        let spare = log.clone();
        drop(spare);
        log.into_inner().expect("one owner")
    }

    #[test]
    fn rewritten_state_behaves_the_same() {
        assert_eq!(counter(), 2);
        assert_eq!(settings(), (vec!["verbose".to_owned()], 1));
        assert_eq!(aliased(), 132);
        assert_eq!(branches(&[4, 8]), 12);
        assert_eq!(branches(&[]), 0);
        assert_eq!(taken().as_deref(), Some("adamantium"));
        assert_eq!(unwrapped(), [1]);
        assert_eq!(timeout(), Duration::from_millis(1));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(deferred()), [1, 2, 3]);
    }
}
