use std::sync::{Arc, RwLock};

fn opaque(value: u32) -> u32 {
    value
}

fn main() {
    let held = Arc::new(RwLock::new(1u32));
    let guard = held.read().unwrap();
    println!("{}", *guard);

    let nested = Arc::new(RwLock::new(1u32));
    let other = Arc::clone(&nested);
    let total = *nested.read().unwrap() + *other.read().unwrap();
    println!("{total}");

    let borrowed = Arc::new(RwLock::new(1u32));
    let view = &*borrowed.read().unwrap();
    println!("{view}");

    let calls = Arc::new(RwLock::new(1u32));
    let count = opaque(*calls.read().unwrap());
    println!("{count}");

    let state_method = Arc::new(RwLock::new(vec![1u32]));
    let count = state_method.read().unwrap().len();
    println!("{count}");

    let try_read = Arc::new(RwLock::new(1u32));
    let result = try_read.try_read().is_ok();
    println!("{result}");
    shadowed_read();
}

fn shadowed_read() {
    trait ReadOverride {
        fn read(&self) -> Result<&'static u32, ()>;
    }

    impl ReadOverride for Arc<RwLock<u32>> {
        fn read(&self) -> Result<&'static u32, ()> {
            Ok(&42)
        }
    }

    let shadowed = Arc::new(RwLock::new(1));
    let count = *shadowed.read().unwrap();
    println!("{count}");
}
