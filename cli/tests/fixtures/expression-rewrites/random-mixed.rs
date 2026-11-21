use std::time::Duration;

use rand::random_range as range;

pub fn token() -> u64 {
    range(0..=1_000_000)
}

pub fn jitter() -> Duration {
    Duration::from_millis(range(0..=250))
}
