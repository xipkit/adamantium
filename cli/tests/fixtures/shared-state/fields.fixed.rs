//! Struct fields that `cargo adamantium fix` rewrites. The test makes this
//! the main file of a binary, applies the fix, compares the result with
//! `fields.fixed.rs`, checks it and runs these tests against it.

#[derive(Clone, Default)]
struct Counter {
    hits: ::adamantium::Actor<u64>,
    log: ::adamantium::Actor<Vec<&'static str>>,
    name: &'static str,
}

impl Counter {
    fn new(name: &'static str) -> Self {
        Self {
            hits: ::adamantium::Actor::new(0),
            log: ::adamantium::Actor::default(),
            name,
        }
    }

    fn hit(&self) {
        self.hits.with(|hits| *hits += 1);
        self.log.with(|log| { (*log).push("hit"); });
    }

    fn total(&self) -> u64 {
        self.hits.read(|hits| *hits)
    }

    fn share(&self) -> Self {
        Self {
            hits: self.hits.clone(),
            log: self.log.clone(),
            name: self.name,
        }
    }
}

fn entries(counter: &Counter) -> usize {
    let log = counter.log.clone();
    let count = log.with(|log| (*log).len());
    count
}

fn seeded(start: u64) -> Counter {
    let hits = ::adamantium::Actor::new(start);
    hits.with(|hits| *hits += 1);
    Counter {
        hits,
        log: ::adamantium::Actor::new(Vec::new()),
        name: "seeded",
    }
}

fn main() {
    let counter = Counter::new("main");
    let shared = counter.share();
    shared.hit();
    let seeded = seeded(1);
    println!(
        "{} {} {} {}",
        counter.name,
        counter.total(),
        entries(&counter),
        seeded.total()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewritten_fields_behave_the_same() {
        let counter = Counter::new("a");
        let shared = counter.share();
        counter.hit();
        shared.hit();
        assert_eq!(counter.total(), 2);
        assert_eq!(entries(&shared), 2);
        let copy = counter.clone();
        copy.hit();
        assert_eq!(counter.total(), 3);
        assert_eq!(seeded(4).total(), 5);
        assert_eq!(Counter::default().total(), 0);
        assert_eq!(entries(&Counter::default()), 0);
    }
}
