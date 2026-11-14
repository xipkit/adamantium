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
