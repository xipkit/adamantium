#![allow(dead_code)]

use std::cell::{Cell, RefCell}; // disallowed_types
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::thread::LocalKey;

use adamantium::{Actor, Array, Str};

thread_local! {
    static NAME: Str = Str::from("worker");
    static COUNT: Cell<usize> = const { Cell::new(0) }; // disallowed_types
    static NAMES: RefCell<Array<Str>> = RefCell::new(Array::new()); // disallowed_types
}

fn counts() -> usize {
    COUNT.set(1); // disallowed_methods
    COUNT.replace(2); // disallowed_methods
    drop(COUNT.take()); // disallowed_methods
    COUNT.get() // disallowed_methods
}

fn names() -> usize {
    NAMES.with_borrow_mut(|names| names.push(Str::from("a"))); // disallowed_methods
    NAMES.with_borrow(|names| names.len()) // disallowed_methods
}

fn name() -> Option<usize> {
    let length = NAME.with(|name| name.len()); // disallowed_methods
    NAME.try_with(|name| name.len()).ok().filter(|other| *other == length) // disallowed_methods
}

fn through(key: &'static LocalKey<Str>) -> usize {
    key.with(|name| name.len()) // disallowed_methods
}

#[allow(clippy::disallowed_methods, reason = "the fixture's local exemption")]
fn exempt() -> usize {
    NAME.with(|name| name.len())
}

static CALLS: AtomicUsize = AtomicUsize::new(0);
static CONFIG: OnceLock<Str> = OnceLock::new();
static DEFAULTS: LazyLock<Array<u8>> = LazyLock::new(|| Array::from([1, 2, 3]));
static REGISTRY: LazyLock<Actor<Array<Str>>> = LazyLock::new(|| Actor::new(Array::new()));

fn shared(stop: &Arc<AtomicBool>) -> usize {
    CALLS.fetch_add(1, Ordering::Relaxed);
    stop.store(true, Ordering::Release);
    let config = CONFIG.get_or_init(|| Str::from("default"));
    REGISTRY.with(|names| names.push(config.clone()));
    DEFAULTS.len()
}
