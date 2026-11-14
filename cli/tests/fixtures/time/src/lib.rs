#![allow(dead_code)]

use std::time::Duration;

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
}

async fn imported_calls() {
    let duration = Duration::from_secs(1);
    let deadline = time::now() + duration;
    sleep(duration).await; // disallowed
    sleep_until(deadline).await; // disallowed
    let _ = interval(duration); // disallowed
    let _ = interval_at(deadline, duration); // disallowed
    let _ = TokioInstant::now(); // disallowed
}

async fn clock_wrappers() {
    let duration = Duration::from_secs(1);
    let deadline = time::now() + duration;
    time::sleep(duration).await;
    time::sleep_until(deadline).await;
    let measured = std::time::Instant::now();
    let _ = measured.elapsed();
}
