// Where `tokio::task::spawn` is defined: a call by the definition's own path
// matches the entry that names its re-export.
pub fn spawn<T>(value: T) -> T {
    value
}

pub fn twice<T>(value: T) -> T {
    spawn(value) // tokio::task::spawn
}
