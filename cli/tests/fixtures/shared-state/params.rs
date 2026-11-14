//! Parameters and return values that `cargo adamantium fix` rewrites. The
//! test makes this the main file of a binary, applies the fix, compares the
//! result with `params.fixed.rs`, checks it and runs these tests against it.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Ledger {
    entries: Arc<Mutex<Vec<u64>>>,
}

impl Ledger {
    fn record(&self, amount: u64) {
        self.append(&self.entries, amount);
    }

    fn append(&self, entries: &Arc<Mutex<Vec<u64>>>, amount: u64) {
        entries.lock().unwrap().push(amount);
    }

    fn balance(&self) -> u64 {
        Self::tally(&self.entries)
    }

    fn tally(entries: &Mutex<Vec<u64>>) -> u64 {
        let total = entries.lock().unwrap().iter().sum();
        total
    }
}

fn counter() -> Arc<Mutex<u64>> {
    Arc::new(Mutex::new(0))
}

fn bump(hits: &Arc<Mutex<u64>>) {
    *hits.lock().unwrap() += 1;
}

fn shared(hits: &Arc<Mutex<u64>>) -> Arc<Mutex<u64>> {
    Arc::clone(hits)
}

fn finish(hits: Arc<Mutex<u64>>) -> u64 {
    let total = *hits.lock().unwrap();
    total
}

fn visits() -> u64 {
    let hits = counter();
    let handle = shared(&hits);
    bump(&hits);
    bump(&handle);
    drop(handle);
    finish(hits)
}

fn main() {
    let ledger = Ledger::default();
    ledger.record(3);
    ledger.clone().record(4);
    println!("{} {}", ledger.balance(), visits());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewritten_parameters_behave_the_same() {
        let ledger = Ledger::default();
        ledger.record(3);
        ledger.clone().record(4);
        assert_eq!(ledger.balance(), 7);
        assert_eq!(visits(), 2);
    }
}
