#![cfg(feature = "tokio")]

use adamantium::{Cancellation, Channel, ReceiveError, SendError, Tasks};
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};
use tokio::time::{Duration, Instant};

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

fn past() -> Option<Instant> {
    Some(Instant::now() - Duration::from_millis(1))
}

#[tokio::test]
async fn receive_until_returns_queued_values_then_closed() {
    let ch = Channel::bounded(2);
    let cancellation = Cancellation::new();
    ch.try_send(1).unwrap();
    ch.try_send(2).unwrap();
    ch.close();
    assert_eq!(ch.receive_until(&cancellation, None).await, Ok(1));
    assert_eq!(ch.receive_until(&cancellation, None).await, Ok(2));
    assert_eq!(
        ch.receive_until(&cancellation, None).await,
        Err(ReceiveError::Closed)
    );
}

#[tokio::test(start_paused = true)]
async fn receive_until_ends_at_the_deadline() {
    let (_sender, receiver) = Channel::<u8>::bounded(1).split();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(5);
    assert_eq!(
        receiver
            .receive_until(&Cancellation::new(), Some(deadline))
            .await,
        Err(ReceiveError::DeadlineExceeded)
    );
    assert!(Instant::now() >= deadline);
}

#[tokio::test]
async fn receive_until_ends_when_cancelled_and_keeps_the_queue() {
    let (sender, receiver) = Channel::bounded(1).split();
    let cancellation = Cancellation::new();
    let counter = Arc::new(WakeCounter::default());
    let mut receive = pin!(receiver.receive_until(&cancellation, None));
    assert!(poll_once(receive.as_mut(), &counter).is_pending());
    cancellation.cancel();
    assert!(counter.wakes() > 0);
    assert_eq!(
        poll_once(receive.as_mut(), &counter),
        Poll::Ready(Err(ReceiveError::Cancelled))
    );

    // A request already present wins over a queued message, which stays.
    sender.try_send(9).unwrap();
    assert_eq!(
        receiver.receive_until(&cancellation, None).await,
        Err(ReceiveError::Cancelled)
    );
    assert_eq!(receiver.try_receive(), Ok(9));
}

#[tokio::test]
async fn receive_until_prefers_cancellation_to_a_passed_deadline() {
    let ch = Channel::<u8>::bounded(1);
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert_eq!(
        ch.receive_until(&cancellation, past()).await,
        Err(ReceiveError::Cancelled)
    );
    assert_eq!(
        ch.receive_until(&Cancellation::new(), past()).await,
        Err(ReceiveError::DeadlineExceeded)
    );
}

#[tokio::test]
async fn send_until_sends_when_there_is_room() {
    let ch = Channel::bounded(1);
    assert_eq!(ch.send_until(&Cancellation::new(), None, 4).await, Ok(()));
    assert_eq!(ch.try_receive(), Ok(4));
}

#[tokio::test(start_paused = true)]
async fn send_until_hands_the_value_back_at_the_deadline() {
    let (sender, receiver) = Channel::bounded(1).split();
    sender.try_send(String::from("queued")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = sender
        .send_until(&Cancellation::new(), Some(deadline), String::from("late"))
        .await;
    assert_eq!(
        result,
        Err(SendError::DeadlineExceeded(String::from("late")))
    );
    assert!(Instant::now() >= deadline);
    assert_eq!(receiver.try_receive().as_deref(), Ok("queued"));
    assert!(receiver.is_empty());
}

#[tokio::test]
async fn send_until_hands_the_value_back_when_cancelled() {
    let ch = Channel::bounded(1);
    ch.try_send(1).unwrap();
    let cancellation = Cancellation::new();
    let counter = Arc::new(WakeCounter::default());
    let mut send = pin!(ch.send_until(&cancellation, None, 2));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    cancellation.cancel();
    assert!(counter.wakes() > 0);
    assert_eq!(
        poll_once(send.as_mut(), &counter),
        Poll::Ready(Err(SendError::Cancelled(2)))
    );
    assert_eq!(ch.len(), 1);
}

#[tokio::test]
async fn send_until_checks_shutdown_before_using_room() {
    let ch = Channel::bounded(1);
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert_eq!(
        ch.send_until(&cancellation, past(), 1).await,
        Err(SendError::Cancelled(1))
    );
    assert_eq!(
        ch.send_until(&Cancellation::new(), past(), 2).await,
        Err(SendError::DeadlineExceeded(2))
    );
    assert!(ch.is_empty());
}

#[tokio::test]
async fn send_until_resumes_when_any_receive_makes_room() {
    let ch = Channel::bounded(1);
    let cancellation = Cancellation::new();
    let counter = Arc::new(WakeCounter::default());
    ch.try_send(0).unwrap();

    let mut send = pin!(ch.send_until(&cancellation, None, 1));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    assert_eq!(ch.try_receive(), Ok(0));
    assert_eq!(counter.wakes(), 1);
    assert_eq!(poll_once(send.as_mut(), &counter), Poll::Ready(Ok(())));

    let mut send = pin!(ch.send_until(&cancellation, None, 2));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    assert_eq!(ch.receive().await, Some(1));
    assert_eq!(counter.wakes(), 2);
    assert_eq!(poll_once(send.as_mut(), &counter), Poll::Ready(Ok(())));

    let mut send = pin!(ch.send_until(&cancellation, None, 3));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    assert_eq!(ch.receive_until(&cancellation, None).await, Ok(2));
    assert_eq!(counter.wakes(), 3);
    assert_eq!(poll_once(send.as_mut(), &counter), Poll::Ready(Ok(())));
    assert_eq!(ch.try_receive(), Ok(3));
}

#[tokio::test]
async fn each_receive_wakes_another_waiting_sender() {
    let ch = Channel::bounded(2);
    let cancellation = Cancellation::new();
    ch.try_send(0).unwrap();
    ch.try_send(1).unwrap();
    let (first_wake, second_wake) = (
        Arc::new(WakeCounter::default()),
        Arc::new(WakeCounter::default()),
    );
    let mut first = pin!(ch.send_until(&cancellation, None, 2));
    let mut second = pin!(ch.send_until(&cancellation, None, 3));
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());

    // Two places open before either woken sender runs.
    assert_eq!(ch.try_receive(), Ok(0));
    assert_eq!(ch.try_receive(), Ok(1));
    assert_eq!(first_wake.wakes(), 1);
    assert_eq!(second_wake.wakes(), 1);
    assert_eq!(poll_once(first.as_mut(), &first_wake), Poll::Ready(Ok(())));
    assert_eq!(
        poll_once(second.as_mut(), &second_wake),
        Poll::Ready(Ok(()))
    );
}

#[tokio::test]
async fn a_dropped_woken_sender_passes_the_wake_on() {
    let ch = Channel::bounded(1);
    let cancellation = Cancellation::new();
    ch.try_send(0).unwrap();
    let (first_wake, second_wake) = (
        Arc::new(WakeCounter::default()),
        Arc::new(WakeCounter::default()),
    );
    let mut first = Box::pin(ch.send_until(&cancellation, None, 1));
    let mut second = pin!(ch.send_until(&cancellation, None, 2));
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());

    assert_eq!(ch.try_receive(), Ok(0));
    assert_eq!(first_wake.wakes(), 1);
    assert_eq!(second_wake.wakes(), 0);
    drop(first);
    assert_eq!(second_wake.wakes(), 1);
    assert_eq!(
        poll_once(second.as_mut(), &second_wake),
        Poll::Ready(Ok(()))
    );
    assert_eq!(ch.try_receive(), Ok(2));
}

#[tokio::test]
async fn send_until_hands_the_value_back_when_the_channel_closes() {
    let cancellation = Cancellation::new();
    let counter = Arc::new(WakeCounter::default());

    let ch = Channel::bounded(1);
    ch.try_send(0).unwrap();
    let mut send = pin!(ch.send_until(&cancellation, None, 1));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    assert!(ch.close());
    assert_eq!(
        poll_once(send.as_mut(), &counter),
        Poll::Ready(Err(SendError::Closed(1)))
    );

    let (sender, receiver) = Channel::bounded(1).split();
    sender.try_send(0).unwrap();
    let mut send = pin!(sender.send_until(&cancellation, None, 2));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    let wakes = counter.wakes();
    drop(receiver);
    assert!(counter.wakes() > wakes);
    assert_eq!(
        poll_once(send.as_mut(), &counter),
        Poll::Ready(Err(SendError::Closed(2)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_producers_and_consumers_without_losing_values() {
    let ch = Channel::<u64>::bounded(2);
    let cancellation = Cancellation::new();
    let result = Tasks::<&str>::run(async |t| {
        let mut producers = Vec::new();
        for id in 0..3 {
            let producer = ch.clone();
            let cancellation = cancellation.clone();
            producers.push(t.spawn(async move {
                let mut n = id;
                loop {
                    match producer.send_until(&cancellation, None, n).await {
                        Ok(()) => n += 3,
                        Err(SendError::Cancelled(value)) => return Ok(value),
                        Err(_) => return Err("unexpected send error"),
                    }
                }
            }));
        }
        let consumer = ch.clone();
        let stopper = cancellation.clone();
        let received = t.spawn(async move {
            let mut received = Vec::new();
            loop {
                match consumer.receive_until(&stopper, None).await {
                    Ok(value) => {
                        received.push(value);
                        if received.len() == 100 {
                            stopper.cancel();
                        }
                    }
                    Err(ReceiveError::Cancelled) => return Ok(received),
                    Err(_) => return Err("unexpected receive error"),
                }
            }
        });
        let received = t.value(received).await?;
        let mut handed_back = Vec::new();
        for producer in producers {
            handed_back.push(t.value(producer).await?);
        }
        Ok((received, handed_back))
    })
    .await;
    let (mut delivered, handed_back) = result.unwrap();
    assert_eq!(delivered.len(), 100);
    while let Ok(value) = ch.try_receive() {
        delivered.push(value);
    }
    delivered.sort_unstable();
    // Each producer sent id, id + 3, ... up to the value it got back, and
    // every sent value was either received or is still queued.
    let mut expected: Vec<u64> = handed_back
        .iter()
        .flat_map(|&stopped| (stopped % 3..stopped).step_by(3))
        .collect();
    expected.sort_unstable();
    assert_eq!(delivered, expected);
}

#[test]
fn errors_describe_themselves_and_hand_values_back() {
    assert_eq!(SendError::Closed(1).into_inner(), 1);
    assert_eq!(SendError::Cancelled(2).into_inner(), 2);
    assert_eq!(SendError::DeadlineExceeded(3).into_inner(), 3);
    assert_eq!(
        SendError::Cancelled(5).to_string(),
        "send cancelled; could not send 5"
    );
    assert_eq!(
        SendError::DeadlineExceeded(6).to_string(),
        "send deadline exceeded; could not send 6"
    );
    assert_eq!(
        SendError::Closed(7).to_string(),
        "the channel is closed; could not send 7"
    );
    assert_eq!(ReceiveError::Cancelled.to_string(), "receive cancelled");
    assert_eq!(
        ReceiveError::DeadlineExceeded.to_string(),
        "receive deadline exceeded"
    );
    assert_eq!(
        ReceiveError::Closed.to_string(),
        "the channel is closed and empty"
    );
}
