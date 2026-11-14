mod spawn;

pub use self::spawn::spawn;

pub fn spawn_blocking<T>(value: T) -> T {
    value
}

pub fn spawn_local<T>(value: T) -> T {
    super::spawn(value) // tokio::spawn
}

/// # Safety
///
/// None needed.
pub unsafe fn raw() {}
