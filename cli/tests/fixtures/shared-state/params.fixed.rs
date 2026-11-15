//! Parameters and return values that `cargo adamantium fix` rewrites. The
//! test makes this the main file of a binary, applies the fix, compares the
//! result with `params.fixed.rs`, checks it and runs these tests against it.

#[derive(Clone, Default)]
struct Ledger {
    entries: ::adamantium::Actor<Vec<u64>>,
}

impl Ledger {
    fn record(&self, amount: u64) {
        self.append(&self.entries, amount);
    }

    fn append(&self, entries: &::adamantium::Actor<Vec<u64>>, amount: u64) {
        entries.with(|entries| { (*entries).push(amount); });
    }

    fn balance(&self) -> u64 {
        Self::tally(&self.entries)
    }

    fn tally(entries: &::adamantium::Actor<Vec<u64>>) -> u64 {
        let total = entries.with(|entries| (*entries).iter().sum());
        total
    }
}

fn counter() -> ::adamantium::Actor<u64> {
    ::adamantium::Actor::new(0)
}

fn bump(hits: &::adamantium::Actor<u64>) {
    hits.with(|hits| *hits += 1);
}

fn shared(hits: &::adamantium::Actor<u64>) -> ::adamantium::Actor<u64> {
    hits.clone()
}

fn finish(hits: ::adamantium::Actor<u64>) -> u64 {
    let total = hits.read(|hits| *hits);
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
