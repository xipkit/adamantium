#![cfg(all(feature = "tokio", not(target_os = "wasi")))]

use std::{
    pin::pin,
    task::Poll,
    time::{Duration, SystemTime},
};

use adamantium::{
    Cancellation, Tasks,
    time::{self, Interval, MissedTickBehavior},
};
use futures_util::poll;

#[tokio::test(start_paused = true)]
async fn now_follows_the_paused_clock() {
    let start = time::now();
    assert_eq!(time::now(), start);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(time::now(), start + Duration::from_secs(5));
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "bounds the reading with the system clock it delegates to"
)]
fn wall_now_reads_the_system_clock() {
    let before = SystemTime::now();
    let wall = time::wall_now();
    assert!(before <= wall && wall <= SystemTime::now());
}

#[tokio::test(start_paused = true)]
#[allow(
    clippy::disallowed_methods,
    reason = "bounds the reading with the system clock it delegates to"
)]
async fn wall_now_ignores_the_paused_clock() {
    let day = Duration::from_secs(60 * 60 * 24);
    let before = SystemTime::now();
    tokio::time::advance(day).await;
    assert!(time::wall_now() < before + day);
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

#[tokio::test(start_paused = true)]
async fn interval_fires_immediately_then_periodically() {
    let start = time::now();
    let period = Duration::from_secs(5);
    let mut interval = Interval::new(period);
    assert_eq!(interval.period(), period);
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Delay);

    assert_eq!(interval.tick().await, start);

    let mut second = pin!(interval.tick());
    assert!(poll!(&mut second).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(poll!(&mut second).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(poll!(&mut second), Poll::Ready(start + period));

    let mut delayed = Interval::at(time::now() + Duration::from_secs(3), period);
    {
        let mut first = pin!(delayed.tick());
        assert!(poll!(&mut first).is_pending());
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(
            poll!(&mut first),
            Poll::Ready(start + Duration::from_secs(8))
        );
    }
    assert_eq!(delayed.tick().await, start + Duration::from_secs(13));
}

#[tokio::test(start_paused = true)]
async fn interval_is_drift_free_within_a_period() {
    for behavior in [MissedTickBehavior::Delay, MissedTickBehavior::Skip] {
        let start = time::now();
        let period = Duration::from_secs(10);
        let mut interval = Interval::at(start + period, period).with_missed_tick_behavior(behavior);

        tokio::time::advance(Duration::from_secs(13)).await;
        assert_eq!(interval.tick().await, start + Duration::from_secs(13));

        let mut next = pin!(interval.tick());
        assert!(poll!(&mut next).is_pending());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(poll!(&mut next).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            poll!(&mut next),
            Poll::Ready(start + Duration::from_secs(20))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn interval_missed_tick_delay_and_skip() {
    let period = Duration::from_secs(10);

    let start = time::now();
    let mut delay = Interval::new(period);
    assert_eq!(delay.tick().await, start);
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!(delay.tick().await, start + Duration::from_secs(25));
    let mut next = pin!(delay.tick());
    assert!(poll!(&mut next).is_pending());
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(poll!(&mut next).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        poll!(&mut next),
        Poll::Ready(start + Duration::from_secs(35))
    );

    let start = time::now();
    let mut skip = Interval::new(period).with_missed_tick_behavior(MissedTickBehavior::Skip);
    assert_eq!(skip.tick().await, start);
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!(skip.tick().await, start + Duration::from_secs(25));
    let mut next = pin!(skip.tick());
    assert!(poll!(&mut next).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(poll!(&mut next).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        poll!(&mut next),
        Poll::Ready(start + Duration::from_secs(30))
    );
}

#[tokio::test(start_paused = true)]
async fn interval_zero_period_does_not_panic() {
    for behavior in [MissedTickBehavior::Delay, MissedTickBehavior::Skip] {
        let start = time::now();
        let mut interval = Interval::new(Duration::ZERO).with_missed_tick_behavior(behavior);
        assert_eq!(interval.tick().await, start);
        assert_eq!(interval.tick().await, start);
    }

    let start = time::now();
    let mut delayed = Interval::at(start + Duration::from_secs(5), Duration::ZERO);
    {
        let mut first = pin!(delayed.tick());
        assert!(poll!(&mut first).is_pending());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            poll!(&mut first),
            Poll::Ready(start + Duration::from_secs(5))
        );
    }
    assert_eq!(delayed.tick().await, start + Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn interval_tick_until_cancels_cleanly_and_wins_over_ready_tick() {
    let start = time::now();
    let cancellation = Cancellation::new();
    let mut interval = Interval::new(Duration::from_secs(5));

    cancellation.cancel();
    assert_eq!(interval.tick_until(&cancellation).await, None);

    let cancellation = Cancellation::new();
    let mut interval = Interval::at(start + Duration::from_secs(5), Duration::from_secs(5));
    assert_eq!(
        {
            let mut fut = pin!(interval.tick_until(&cancellation));
            assert!(poll!(&mut fut).is_pending());
            tokio::time::advance(Duration::from_secs(2)).await;
            assert!(poll!(&mut fut).is_pending());
            cancellation.cancel();
            poll!(&mut fut)
        },
        Poll::Ready(None)
    );
    assert_eq!(time::now(), start + Duration::from_secs(2));

    let active = Cancellation::new();
    assert_eq!(
        interval.tick_until(&active).await,
        Some(start + Duration::from_secs(5))
    );
}

#[tokio::test(start_paused = true)]
async fn interval_reset_and_reset_at_reschedule_next_tick() {
    let start = time::now();
    let mut interval = Interval::new(Duration::from_secs(10));
    assert_eq!(interval.tick().await, start);
    tokio::time::advance(Duration::from_secs(4)).await;
    interval.reset();
    assert_eq!(interval.tick().await, start + Duration::from_secs(14));

    interval.reset_at(time::now() + Duration::from_secs(3));
    assert_eq!(interval.tick().await, start + Duration::from_secs(17));
}

#[tokio::test]
async fn interval_zero_period_tick_until_cooperates_on_current_thread() {
    let shutdown = Cancellation::new();
    let worker_shutdown = shutdown.clone();
    Tasks::run(async move |t| {
        let mut ticks = 0u64;
        t.spawn(async move {
            let mut interval = Interval::new(Duration::ZERO);
            while interval.tick_until(&worker_shutdown).await.is_some() {
                ticks += 1;
            }
            assert!(ticks > 0);
            Ok::<(), ()>(())
        });
        tokio::task::yield_now().await;
        shutdown.cancel();
        Ok(())
    })
    .await
    .unwrap();
}
