use std::sync::{Arc, RwLock as Lock};

struct Counter {
    value: Arc<Lock<u32>>,
}

impl Counter {
    fn increment(&self) {
        *self.value.write().unwrap() += 1;
    }
}

fn fresh() -> Arc<Lock<u32>> {
    Arc::new(Lock::new(41))
}

fn read_value(lock: &Lock<u32>) -> u32 {
    *lock.read().expect("counter poisoned")
}

fn main() {
    let value = fresh();
    let counter = Counter { value: Arc::clone(&value) };
    counter.increment();
    let snapshot = read_value(&value);
    let local: Arc<Lock<u32>> = Arc::new(Lock::new(2));
    let count = *local.read().unwrap();
    println!("{snapshot} {count}");
}
