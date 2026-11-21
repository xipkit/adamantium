use std::time::Duration;

pub fn session_token() -> u64 {
    rand::random::<u64>()
}

pub fn nonce() -> u64 {
    let nonce: u64 = rand::random_range(0..=1_000_000);
    nonce
}

pub fn offset() -> Duration {
    Duration::from_millis(1 + rand::random_range(0..=7_u64))
}

pub fn capped() -> Duration {
    Duration::from_millis(rand::random::<u64>().min(250))
}

pub fn passed_on() -> Duration {
    Duration::from_millis(identity(rand::random_range(0..=7_u64)))
}

pub fn subsecond() -> Duration {
    Duration::new(0, rand::random_range(0..=999))
}

pub fn fractional() -> Duration {
    Duration::from_secs_f64(rand::random())
}

fn identity(value: u64) -> u64 {
    value
}
