// Every test here pauses the clock, which Tokio cannot do on WASI.
#![cfg(all(feature = "tokio", not(target_os = "wasi")))]

use std::{pin::pin, task::Poll, time::Duration};

use adamantium::time;
use futures_util::poll;

#[tokio::test(start_paused = true)]
async fn now_follows_the_paused_clock() {
    let start = time::now();
    assert_eq!(time::now(), start);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(time::now(), start + Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn sleep_waits_for_the_duration_on_the_paused_clock() {
    let start = time::now();
    let mut sleep = pin!(time::sleep(Duration::from_secs(5)));
    assert!(poll!(&mut sleep).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(poll!(&mut sleep).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(poll!(&mut sleep), Poll::Ready(()));
    assert_eq!(time::now(), start + Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn sleep_until_keeps_its_deadline_when_polled_later() {
    let deadline = time::now() + Duration::from_secs(5);
    let mut sleep = pin!(time::sleep_until(deadline));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(poll!(&mut sleep).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(poll!(&mut sleep).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(poll!(&mut sleep), Poll::Ready(()));
    assert_eq!(time::now(), deadline);
}

#[tokio::test(start_paused = true)]
async fn sleep_until_a_past_deadline_is_ready() {
    let mut sleep = pin!(time::sleep_until(time::now() - Duration::from_secs(1)));
    assert_eq!(poll!(&mut sleep), Poll::Ready(()));
}
