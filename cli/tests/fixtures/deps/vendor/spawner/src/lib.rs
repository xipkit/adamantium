//! Each call to a disallowed path ends with a comment naming the entry it
//! matches, or `unresolved` for a call counted by name.

mod dir;
#[path = "elsewhere/renamed.rs"]
mod renamed;
mod globbed;
#[path = "../shared.rs"]
mod shared;
mod nested;
mod missing;
#[cfg(test)]
mod tests_only;
#[cfg(feature = "off")]
mod off;
#[cfg(target_pointer_width = "8")]
mod tiny;
#[cfg(spawner_custom)]
mod custom;
mod declared;

pub mod shim {
    pub use std::thread;
}

pub mod glob_shim {
    pub use std::thread::*;
}

use rt::task::{self, spawn_blocking as blocking};

macro_rules! cfg_on {
    ($($item:item)*) => {
        $(#[cfg(feature = "on")] $item)*
    };
}

macro_rules! raw {
    () => {
        unsafe { spawn(1) } // unresolved
    };
}

pub fn imported() {
    rt::spawn(()); // tokio::spawn
    task::spawn(()); // tokio::task::spawn
    blocking(()); // tokio::task::spawn_blocking
}

pub fn standard(value: Box<u8>, shared: &std::sync::Arc<u8>) {
    core::mem::forget(1); // std::mem::forget
    Box::leak(value); // std::boxed::Box::leak
    std::sync::Arc::downgrade(shared); // std::sync::Arc::downgrade
}

pub fn scoped() {
    use std::thread;
    thread::spawn(|| ()); // std::thread::spawn
}

pub fn through_a_module() {
    crate::shim::thread::sleep(std::time::Duration::ZERO); // std::thread::sleep
}

pub fn through_a_glob() {
    crate::glob_shim::spawn(|| ()); // unresolved
}

pub fn method(handle: rt::Handle) {
    handle.spawn(); // unresolved
}

pub fn another_crate() {
    pool::spawn(()); // unresolved
}

pub fn local() {
    fn spawn() {}
    spawn();
}

#[cfg(feature = "on")]
pub fn on() {
    rt::task::spawn_local(()); // tokio::task::spawn_local
}

#[cfg(not(feature = "on"))]
pub fn not_on() {
    rt::task::spawn_local(());
}

#[cfg(not(feature = "dev"))]
pub fn without_dev() {
    rt::spawn(()); // tokio::spawn
}

cfg_on! {
    pub fn expanded() {
        task::spawn(()); // tokio::task::spawn
    }
}

pub fn nested_cfg() -> [(); 1] {
    [
        #[cfg(any())]
        unsafe {
            rt::spawn(())
        },
        #[cfg(all())]
        task::spawn(()), // tokio::task::spawn
    ]
}

pub fn with_a_local_module() {
    #[path = "local/child.rs"]
    mod child;
    child::run();
}

// Parameters and locals named `blocking` shadow the import.
pub fn shadowed(blocking: fn(())) {
    blocking(());
}

pub fn shadowed_by_patterns(callbacks: Option<fn(())>) {
    if let Some(blocking) = callbacks {
        blocking(());
    }
    match callbacks {
        Some(blocking) => blocking(()),
        None => {}
    }
    for blocking in callbacks {
        blocking(());
    }
    let _ = |blocking: fn(())| blocking(());
    {
        let blocking = |_: ()| ();
        blocking(());
    }
    blocking(()); // tokio::task::spawn_blocking
}

pub enum Discriminants {
    #[cfg(any())]
    Disabled = unsafe { rt::spawn(0) },
    Enabled = 1,
}

pub struct Fields {
    #[cfg(any())]
    pub disabled: [u8; unsafe { rt::spawn(0) }],
    pub enabled: u8,
}

// A `let` in a condition binds its names after its initializer runs.
pub fn condition_order() {
    if let Some(blocking) = Some(blocking(())) { // tokio::task::spawn_blocking
        blocking;
    }
    while let Some(blocking) = Some(blocking(())) { // tokio::task::spawn_blocking
        blocking;
    }
}

// Generic parameters named like prelude types are not the prelude's.
pub trait Safe {
    fn insert();
    fn leak();
}

pub fn generic<Vec: Safe>() {
    Vec::insert();
}

pub struct Holder<T>(T);

impl<Box: Safe> Holder<Box> {
    pub fn leak() {
        Box::leak();
    }
}

pub fn blocks() {
    unsafe { rt::task::raw() };
    println!("{:?}", task::spawn(())); // tokio::task::spawn
}

/// # Safety
///
/// None needed.
pub unsafe fn unsafe_fn() {}

/// # Safety
///
/// None needed.
pub unsafe trait UnsafeTrait {}

unsafe impl UnsafeTrait for u8 {}

// A `#[test]` function is built only for tests, like `#[cfg(test)]`.
#[test]
fn standalone_test() {
    unsafe { rt::spawn(()) };
}

#[cfg_attr(all(), test)]
fn conditional_test() {
    unsafe { rt::spawn(()) };
}
