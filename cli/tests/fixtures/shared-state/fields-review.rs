//! Struct fields that `cargo adamantium fix` leaves for review. Each struct
//! holds one field that a mechanical rewrite can't follow everywhere it is
//! used; the test checks that the file is unchanged and that each is
//! reported with its section of the migration guide.

use std::sync::{Arc, Mutex};

macro_rules! touch {
    ($state:expr) => {
        drop($state)
    };
}

/// Locked in a closure, which might run inside an actor's callback.
#[derive(Default)]
struct Deferred {
    deferred: Arc<Mutex<u32>>,
}

impl Deferred {
    fn bump(&self) -> impl Fn() + '_ {
        || *self.deferred.lock().unwrap() += 1
    }
}

/// Locked in async code, which fix doesn't follow yet.
#[derive(Default)]
struct Awaited {
    awaited: Arc<Mutex<u32>>,
}

impl Awaited {
    async fn bump(&self) {
        *self.awaited.lock().unwrap() += 1;
    }
}

/// Locked when dropped, which can happen inside an actor's callback.
#[derive(Default)]
struct Dropped {
    dropped: Arc<Mutex<u32>>,
}

impl Drop for Dropped {
    fn drop(&mut self) {
        *self.dropped.lock().unwrap() += 1;
    }
}

/// Named in a macro fix can't read.
#[derive(Default)]
struct Touched {
    touched: Arc<Mutex<u32>>,
}

impl Touched {
    fn release(self) {
        touch!(self.touched);
    }
}

/// Formatted, which an actor does differently.
#[derive(Debug, Default)]
struct Printed {
    printed: Arc<Mutex<u32>>,
}

impl Printed {
    fn bump(&self) {
        *self.printed.lock().unwrap() += 1;
    }
}

fn main() {
    Deferred::default().bump()();
    drop(Awaited::default().bump());
    drop(Dropped::default());
    Touched::default().release();
    let printed = Printed::default();
    printed.bump();
    println!("{printed:?}");
}
