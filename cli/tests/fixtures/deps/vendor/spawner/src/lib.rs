//! Each call to a disallowed path ends with a comment naming the entry it
//! matches, or `unresolved` for a call counted by name.

mod dir;
#[path = "elsewhere/renamed.rs"]
mod renamed;
mod globbed;
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
