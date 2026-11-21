pub mod time {
    use std::time::Duration;

    pub fn jitter() -> Duration {
        Duration::from_millis(250)
    }
}

struct Duration(u64);

impl Duration {
    fn from_millis(value: u64) -> Self {
        Self(value)
    }
}

pub fn key() -> u64 {
    Duration::from_millis(rand::random::<u64>()).0
}
