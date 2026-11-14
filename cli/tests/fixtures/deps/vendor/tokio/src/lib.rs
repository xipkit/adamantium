//! Stands in for Tokio, whose own code reaches its spawns through `crate`
//! and `super`.

pub mod task;

pub use task::spawn;

pub fn run() {
    crate::task::spawn(()); // tokio::task::spawn
    self::task::spawn(()); // tokio::task::spawn
}

pub struct Handle;

impl Handle {
    pub fn spawn(&self) {}
}

/// Stands in for psm's `psm_stack_manipulation!`, which a crate calls with
/// `#[macro_use] extern crate`: a `cfg` that the build script sets picks one
/// of two definitions, so either may be built.
#[cfg(tokio_custom)]
#[macro_export]
macro_rules! runtime {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => {
        $($yes)*
        pub use $crate::task::spawn as start;
    };
}

#[cfg(not(tokio_custom))]
#[macro_export]
macro_rules! runtime {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => {
        $($no)*
        pub use $crate::task::spawn as start;
    };
}
