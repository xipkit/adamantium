#![allow(dead_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adamantium::time;
use tokio::time::{Instant as TokioInstant, interval, interval_at, sleep, sleep_until};

async fn direct_calls() {
    let duration = Duration::from_secs(1);
    let deadline = time::now() + duration;
    std::thread::sleep(duration); // disallowed
    tokio::time::sleep(duration).await; // disallowed
    tokio::time::sleep_until(deadline).await; // disallowed
    let _ = tokio::time::interval(duration); // disallowed
    let _ = tokio::time::interval_at(deadline, duration); // disallowed
    let _ = tokio::time::Instant::now(); // disallowed
    let _ = std::time::SystemTime::now(); // disallowed
}

async fn imported_calls() {
    let duration = Duration::from_secs(1);
    let deadline = time::now() + duration;
    sleep(duration).await; // disallowed
    sleep_until(deadline).await; // disallowed
    let _ = interval(duration); // disallowed
    let _ = interval_at(deadline, duration); // disallowed
    let _ = TokioInstant::now(); // disallowed
    let _ = SystemTime::now(); // disallowed
}

async fn clock_wrappers() {
    let duration = Duration::from_secs(1);
    let deadline = time::now() + duration;
    time::sleep(duration).await;
    time::sleep_until(deadline).await;
    let measured = std::time::Instant::now();
    let _ = measured.elapsed();
}

fn expired(exp_secs: u64) -> bool {
    time::wall_now() >= UNIX_EPOCH + Duration::from_secs(exp_secs)
}

fn log_timestamp() -> Duration {
    #[expect(
        clippy::disallowed_methods,
        reason = "the timestamp only goes into a log line"
    )]
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).unwrap_or_default()
}
