#[cfg(feature = "tokio")]
mod support;

#[cfg(feature = "tokio")]
use adamantium::{Cancellation, FeedError, Tasks, time};
use adamantium::{Closed, Ended, Fanout, Feed, Sent};
#[cfg(feature = "tokio")]
use std::time::Duration;
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl WakeCounter {
    fn wakes(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>, counter: &Arc<WakeCounter>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::clone(counter));
    future.poll(&mut Context::from_waker(&waker))
}

fn now<F: Future>(future: F) -> Poll<F::Output> {
    poll_once(pin!(future), &Arc::new(WakeCounter::default()))
}

fn drain<T>(feed: &mut Feed<T>) -> (Vec<T>, Option<Ended>) {
    let mut events = Vec::new();
    loop {
        match feed.try_receive() {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return (events, None),
            Err(end) => return (events, Some(end)),
        }
    }
}

fn sent(delivered: usize, disconnected: usize) -> Result<Sent, Closed<u32>> {
    Ok(Sent {
        delivered,
        disconnected,
    })
}

#[test]
fn a_stalled_feed_is_disconnected_and_a_healthy_one_gets_every_event() {
    let fanout = Fanout::new(2);
    let mut healthy = fanout.subscribe();
    let mut stalled = fanout.subscribe();
    let mut received = Vec::new();
    let mut reports = Vec::new();
    for n in 1..=5 {
        reports.push(fanout.send(n));
        received.push(healthy.try_receive());
        if n == 2 {
            assert_eq!(fanout.subscriber_count(), 2);
            assert!(!stalled.is_ended());
        }
    }
    assert_eq!(
        reports,
        [sent(2, 0), sent(2, 0), sent(1, 1), sent(1, 0), sent(1, 0)]
    );
    assert_eq!(received, (1..=5).map(|n| Ok(Some(n))).collect::<Vec<_>>());
    assert_eq!(healthy.try_receive(), Ok(None));
    assert!(!healthy.is_ended());
    assert_eq!(fanout.subscriber_count(), 1);

    assert!(stalled.is_ended(), "ended with events still queued");
    assert_eq!(stalled.len(), 2);
    assert_eq!(drain(&mut stalled), (vec![1, 2], Some(Ended::Overflowed)));
    assert_eq!(stalled.try_receive(), Err(Ended::Overflowed), "ends stay");
    assert_eq!(now(stalled.receive()), Poll::Ready(Err(Ended::Overflowed)));
}

#[test]
fn a_feed_with_room_left_is_not_disconnected() {
    let fanout = Fanout::new(2);
    let mut feed = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(1, 0));
    assert_eq!(fanout.send(2), sent(1, 0));
    assert_eq!(feed.try_receive(), Ok(Some(1)));
    assert_eq!(fanout.send(3), sent(1, 0), "reading one made room");
    assert_eq!(drain(&mut feed), (vec![2, 3], None));
}

#[test]
fn a_capacity_of_zero_queues_one_event() {
    let fanout = Fanout::new(0);
    let mut feed = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(1, 0));
    assert_eq!(fanout.send(2), sent(0, 1));
    assert_eq!(drain(&mut feed), (vec![1], Some(Ended::Overflowed)));
}

#[test]
fn a_feed_receives_only_what_is_sent_after_it_subscribes() {
    let fanout = Fanout::new(4);
    let mut early = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(1, 0));
    let mut late = fanout.subscribe();
    assert_eq!(fanout.send(2), sent(2, 0));
    assert_eq!(drain(&mut early), (vec![1, 2], None));
    assert_eq!(drain(&mut late), (vec![2], None));
}

#[test]
fn a_send_with_no_feeds_delivers_to_nobody() {
    let fanout = Fanout::new(1);
    assert_eq!(fanout.send(1), sent(0, 0));
    assert_eq!(fanout.subscriber_count(), 0);
}

#[test]
fn a_resubscribed_feed_starts_from_the_next_event() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(1, 0));
    assert_eq!(fanout.send(2), sent(0, 1));
    let mut fresh = feed.resubscribe();
    assert_eq!(fresh.try_receive(), Ok(None));
    assert_eq!(fanout.subscriber_count(), 1);
    assert_eq!(fanout.send(3), sent(1, 0));
    assert_eq!(drain(&mut fresh), (vec![3], None));
    assert_eq!(drain(&mut feed), (vec![1], Some(Ended::Overflowed)));

    let mut copy = fresh.resubscribe();
    assert_eq!(fanout.send(4), sent(2, 0));
    assert_eq!(drain(&mut fresh), (vec![4], None));
    assert_eq!(drain(&mut copy), (vec![4], None));
}

#[test]
fn closing_drains_each_feed_and_then_ends_it() {
    let fanout = Fanout::new(4);
    let mut feed = fanout.subscribe();
    let mut waiting = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(2, 0));
    assert_eq!(waiting.try_receive(), Ok(Some(1)));

    let counter = Arc::new(WakeCounter::default());
    let mut wait = pin!(waiting.receive());
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Pending);
    assert!(fanout.close());
    assert!(counter.wakes() > 0, "closing woke the wait");
    assert_eq!(
        poll_once(wait.as_mut(), &counter),
        Poll::Ready(Err(Ended::Closed))
    );

    assert!(!fanout.close());
    assert!(fanout.is_closed());
    assert_eq!(fanout.subscriber_count(), 0);
    assert_eq!(fanout.send(2), Err(Closed(2)));
    assert!(feed.is_ended());
    assert_eq!(drain(&mut feed), (vec![1], Some(Ended::Closed)));
    assert_eq!(now(feed.receive()), Poll::Ready(Err(Ended::Closed)));
}

#[test]
fn a_feed_made_after_closing_has_already_ended() {
    let fanout = Fanout::<u32>::new(1);
    let feed = fanout.subscribe();
    fanout.close();
    for mut late in [fanout.subscribe(), feed.resubscribe()] {
        assert!(late.is_ended());
        assert_eq!(late.try_receive(), Err(Ended::Closed));
    }
    assert_eq!(fanout.subscriber_count(), 0);
}

#[test]
fn an_overflowed_feed_still_reports_overflow_after_closing() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    assert_eq!(fanout.send(1), sent(1, 0));
    assert_eq!(fanout.send(2), sent(0, 1));
    fanout.close();
    assert_eq!(drain(&mut feed), (vec![1], Some(Ended::Overflowed)));
}

#[test]
fn dropping_the_last_fanout_closes_it_and_feeds_dont_hold_it_open() {
    let fanout = Fanout::new(2);
    let other = fanout.clone();
    let mut feed = fanout.subscribe();
    let counter = Arc::new(WakeCounter::default());

    drop(fanout);
    assert!(!other.is_closed(), "a clone still holds it open");
    assert_eq!(other.send(1), sent(1, 0));
    assert_eq!(feed.try_receive(), Ok(Some(1)));

    let mut wait = pin!(feed.receive());
    assert_eq!(poll_once(wait.as_mut(), &counter), Poll::Pending);
    drop(other);
    assert!(counter.wakes() > 0);
    assert_eq!(
        poll_once(wait.as_mut(), &counter),
        Poll::Ready(Err(Ended::Closed))
    );
}

#[test]
fn dropping_a_feed_unsubscribes_it_at_once() {
    let fanout = Fanout::new(1);
    let kept = fanout.subscribe();
    let dropped = fanout.subscribe();
    assert_eq!(fanout.subscriber_count(), 2);
    drop(dropped);
    assert_eq!(fanout.subscriber_count(), 1);
    assert_eq!(fanout.send(1), sent(1, 0));
    drop(kept);
    assert_eq!(fanout.subscriber_count(), 0);
    assert!(!fanout.is_closed());
}

#[test]
fn sends_from_every_clone_reach_every_feed_in_one_order() {
    let first = Fanout::new(16);
    let second = first.clone();
    let mut feeds = [first.subscribe(), second.subscribe(), first.subscribe()];
    for n in 0..8 {
        let publisher = if n % 3 == 0 { &first } else { &second };
        assert_eq!(publisher.send(n), sent(3, 0));
    }
    for feed in &mut feeds {
        assert_eq!(drain(feed), ((0..8).collect(), None));
    }
}

#[test]
fn a_send_wakes_each_waiting_feed() {
    let fanout = Fanout::new(1);
    let mut first = fanout.subscribe();
    let mut second = fanout.subscribe();
    let counters = [
        Arc::new(WakeCounter::default()),
        Arc::new(WakeCounter::default()),
    ];
    let mut first_waits = pin!(first.receive());
    let mut second_waits = pin!(second.receive());
    assert_eq!(poll_once(first_waits.as_mut(), &counters[0]), Poll::Pending);
    assert_eq!(
        poll_once(second_waits.as_mut(), &counters[1]),
        Poll::Pending
    );
    fanout.send(7).unwrap();
    assert!(counters.iter().all(|counter| counter.wakes() > 0));
    assert_eq!(
        poll_once(first_waits.as_mut(), &counters[0]),
        Poll::Ready(Ok(7))
    );
    assert_eq!(
        poll_once(second_waits.as_mut(), &counters[1]),
        Poll::Ready(Ok(7))
    );
}

#[test]
fn dropping_a_waiting_receive_loses_nothing() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    let counter = Arc::new(WakeCounter::default());
    {
        let mut waiting = pin!(feed.receive());
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
        fanout.send(1).unwrap();
    }
    assert_eq!(feed.try_receive(), Ok(Some(1)));
}

#[derive(Debug, PartialEq)]
struct Fragile(u32);

const PANICS: u32 = 13;

impl Clone for Fragile {
    fn clone(&self) -> Self {
        if self.0 == PANICS {
            std::panic::panic_any("clone failed");
        }
        Self(self.0)
    }
}

#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
#[test]
fn a_clone_that_panics_delivers_the_event_to_no_feed() {
    let fanout = Fanout::new(2);
    let mut feeds = [fanout.subscribe(), fanout.subscribe(), fanout.subscribe()];
    let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(fanout.send(Fragile(PANICS)));
    }));
    assert!(sent.is_err());
    assert_eq!(
        fanout.send(Fragile(1)),
        Ok(Sent {
            delivered: 3,
            disconnected: 0
        }),
        "the fanout still works"
    );
    for feed in &mut feeds {
        assert_eq!(drain(feed), (vec![Fragile(1)], None));
    }
}

#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
#[test]
fn a_fanout_dropped_by_a_panic_still_closes() {
    let fanout = Fanout::<u32>::new(1);
    let mut feed = fanout.subscribe();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = fanout;
        std::panic::panic_any("unwinding");
    }));
    assert!(unwound.is_err());
    assert_eq!(feed.try_receive(), Err(Ended::Closed));
}

struct ReadsOnDrop(Option<Fanout<ReadsOnDrop>>);

impl Clone for ReadsOnDrop {
    fn clone(&self) -> Self {
        Self(None)
    }
}

impl Drop for ReadsOnDrop {
    fn drop(&mut self) {
        if let Some(fanout) = self.0.take() {
            // Takes the fanout's lock, so it would deadlock if the fanout
            // dropped an event while holding it.
            let _ = fanout.subscriber_count();
        }
    }
}

#[test]
fn events_are_dropped_outside_the_lock() {
    let fanout = Fanout::new(1);
    let reads = || ReadsOnDrop(Some(fanout.clone()));
    assert!(fanout.send(reads()).is_ok());

    let feed = fanout.subscribe();
    assert!(fanout.send(reads()).is_ok());
    drop(feed);

    let overflowed = fanout.subscribe();
    assert!(fanout.send(reads()).is_ok());
    assert!(fanout.send(ReadsOnDrop(None)).is_ok());
    assert!(overflowed.is_ended());
    drop(overflowed);

    let closed = fanout.subscribe();
    assert!(fanout.send(reads()).is_ok());
    assert!(fanout.close());
    drop(closed);
}

struct ReadsOnWake(Fanout<u32>);

impl Wake for ReadsOnWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Takes the fanout's lock, so it would deadlock if a send woke the
        // receive while holding it.
        let _ = self.0.subscriber_count();
    }
}

#[test]
fn feeds_are_woken_outside_the_lock() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    let waker = Waker::from(Arc::new(ReadsOnWake(fanout.clone())));
    let mut waiting = pin!(feed.receive());
    assert_eq!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    );
    assert_eq!(fanout.send(1), sent(1, 0));
    assert_eq!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Ok(1))
    );
}

struct ExplodesOnSecondClone {
    fanout: Arc<Fanout<ExplodesOnSecondClone>>,
    clones: Arc<AtomicUsize>,
}

impl Clone for ExplodesOnSecondClone {
    fn clone(&self) -> Self {
        if self.clones.fetch_add(1, Ordering::SeqCst) == 1 {
            std::panic::panic_any("second clone failed");
        }
        Self {
            fanout: Arc::clone(&self.fanout),
            clones: Arc::clone(&self.clones),
        }
    }
}

impl Drop for ExplodesOnSecondClone {
    fn drop(&mut self) {
        // Takes the fanout's lock, so it would deadlock if the clone made
        // before the panic were dropped while the send held it.
        let _ = self.fanout.subscriber_count();
    }
}

#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
#[test]
fn clones_made_before_a_panicking_clone_drop_outside_the_lock() {
    let fanout = Arc::new(Fanout::new(1));
    let mut feeds = [fanout.subscribe(), fanout.subscribe(), fanout.subscribe()];
    let clones = Arc::new(AtomicUsize::new(0));
    let event = ExplodesOnSecondClone {
        fanout: Arc::clone(&fanout),
        clones: Arc::clone(&clones),
    };
    let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(fanout.send(event));
    }));
    assert!(sent.is_err());
    assert_eq!(clones.load(Ordering::SeqCst), 2);
    for feed in &mut feeds {
        assert_eq!(feed.try_receive().map(|event| event.is_some()), Ok(false));
    }
    assert_eq!(fanout.subscriber_count(), 3);
}

#[test]
fn handles_cross_tasks_and_describe_themselves() {
    fn crosses_tasks<T: Send + Sync + 'static>() {}
    crosses_tasks::<Fanout<String>>();
    crosses_tasks::<Feed<String>>();
    crosses_tasks::<Ended>();
    #[cfg(feature = "tokio")]
    crosses_tasks::<FeedError>();

    let fanout = Fanout::new(0);
    let feed = fanout.subscribe();
    assert_eq!(
        format!("{fanout:?}"),
        "Fanout { subscribers: 1, capacity: 1, closed: false }"
    );
    fanout.send("event").unwrap();
    fanout.send("missed").unwrap();
    assert_eq!(
        format!("{feed:?}"),
        "Feed { len: 1, ended: Some(Overflowed) }"
    );
    assert_eq!(
        Ended::Overflowed.to_string(),
        "the feed fell behind and was disconnected, and is drained"
    );
}

#[cfg(feature = "tokio")]
fn past() -> Option<Instant> {
    Some(time::now() - Duration::from_millis(1))
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_at_the_deadline_and_leaves_later_events() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    let cancellation = Cancellation::new();
    let start = time::now();
    assert_eq!(
        feed.receive_until(&cancellation, Some(start + Duration::from_secs(5)))
            .await,
        Err(FeedError::DeadlineExceeded)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    fanout.send(1).unwrap();
    fanout.send(2).unwrap();
    assert_eq!(feed.receive_until(&cancellation, None).await, Ok(1));
    assert_eq!(
        feed.receive_until(&cancellation, None).await,
        Err(FeedError::Overflowed)
    );
    let mut fresh = feed.resubscribe();
    fanout.close();
    assert_eq!(
        fresh.receive_until(&cancellation, None).await,
        Err(FeedError::Closed)
    );
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_when_cancelled() {
    let fanout = Fanout::<u32>::new(1);
    let mut feed = fanout.subscribe();
    let cancellation = Cancellation::new();
    let start = time::now();
    let canceller = cancellation.clone();
    let (received, ()) = tokio::join!(
        feed.receive_until(&cancellation, Some(start + Duration::from_secs(3600))),
        async move {
            time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        },
    );
    assert_eq!(received, Err(FeedError::Cancelled));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn receive_until_prefers_shutdown_to_a_queued_event() {
    let fanout = Fanout::new(1);
    let mut feed = fanout.subscribe();
    fanout.send(1).unwrap();
    let cancellation = Cancellation::new();
    for _ in 0..16 {
        assert_eq!(
            feed.receive_until(&cancellation, past()).await,
            Err(FeedError::DeadlineExceeded)
        );
    }
    cancellation.cancel();
    for _ in 0..16 {
        assert_eq!(
            feed.receive_until(&cancellation, past()).await,
            Err(FeedError::Cancelled)
        );
    }
    assert_eq!(feed.try_receive(), Ok(Some(1)), "the event stayed queued");
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publishers_reach_every_feed_in_one_order() {
    const EACH: u32 = 2_000;
    let fanout = Fanout::new(usize::try_from(2 * EACH).unwrap());
    let orders = Tasks::<&str>::run(async |t| {
        let mut readers = Vec::new();
        for _ in 0..3 {
            let mut feed = fanout.subscribe();
            readers.push(t.spawn(async move {
                let mut order = Vec::new();
                loop {
                    match feed.receive().await {
                        Ok(event) => order.push(event),
                        Err(Ended::Closed) => return Ok(order),
                        Err(Ended::Overflowed) => return Err("a feed overflowed"),
                    }
                }
            }));
        }
        let mut writers = Vec::new();
        for publisher in 0..2 {
            let fanout = fanout.clone();
            writers.push(t.spawn(async move {
                for n in 0..EACH {
                    fanout.send((publisher, n)).map_err(|_| "closed")?;
                    tokio::task::yield_now().await;
                }
                Ok(())
            }));
        }
        for writer in writers {
            t.value(writer).await?;
        }
        fanout.close();
        let mut orders = Vec::new();
        for reader in readers {
            orders.push(t.value(reader).await?);
        }
        Ok(orders)
    })
    .await
    .unwrap();

    let first = &orders[0];
    assert!(orders.iter().all(|order| order == first), "one order");
    for publisher in 0..2 {
        let own: Vec<u32> = first
            .iter()
            .filter(|(from, _)| *from == publisher)
            .map(|(_, n)| *n)
            .collect();
        assert_eq!(own, (0..EACH).collect::<Vec<_>>(), "in program order");
    }
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_task_never_holds_up_a_healthy_one() {
    let fanout = Fanout::new(4);
    let stalled = fanout.subscribe();
    let result = Tasks::<&str>::run(async |t| {
        let (acks, acked) = adamantium::Channel::bounded(1).split();
        let mut healthy = fanout.subscribe();
        let reader = t.spawn(async move {
            let mut expected = 0_u32;
            loop {
                match healthy.receive().await {
                    Ok(event) if event == expected => {
                        expected += 1;
                        acks.send(()).await.map_err(|_| "acks closed")?;
                    }
                    Ok(_) => return Err("an event out of order"),
                    Err(Ended::Closed) => return Ok(expected),
                    Err(Ended::Overflowed) => return Err("the healthy feed overflowed"),
                }
            }
        });
        let publisher = fanout.clone();
        t.spawn(async move {
            for n in 0..1_000 {
                let report = publisher.send(n).map_err(|_| "closed")?;
                let expected = if n == 4 { 1 } else { 0 };
                if report.disconnected != expected {
                    return Err("disconnected the wrong feeds");
                }
                support::timeout(Duration::from_secs(5), acked.receive())
                    .await
                    .map_err(|_| "the healthy reader stalled")?;
            }
            publisher.close();
            Ok(())
        });
        t.value(reader).await
    })
    .await;
    assert_eq!(result, Ok(1_000));
    let mut stalled = stalled;
    assert_eq!(
        drain(&mut stalled),
        (vec![0, 1, 2, 3], Some(Ended::Overflowed))
    );
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_racing_a_new_wait_is_never_missed() {
    // A wait that checks, then listens, can miss a send made between the
    // two and sleep forever. Each round races a send against a fresh wait.
    let fanout = Fanout::new(1);
    let result = Tasks::<&str>::run(async |t| {
        for round in 1..=20_000_u32 {
            let mut feed = fanout.subscribe();
            let waiting = t.spawn(async move { Ok(feed.receive().await) });
            let publisher = fanout.clone();
            t.spawn(async move { publisher.send(round).map(drop).map_err(|_| "closed") });
            let received = support::timeout(Duration::from_secs(5), t.value(waiting))
                .await
                .map_err(|_| "a wait missed its send")??;
            if received != Ok(round) {
                return Err("received the wrong event");
            }
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
}
