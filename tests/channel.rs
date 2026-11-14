#[cfg(feature = "tokio")]
mod support;

#[cfg(feature = "tokio")]
use adamantium::{Actor, Tasks};
use adamantium::{Channel, Closed, TryReceiveError, TrySendError};
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

#[derive(Default)]
struct WakeCounter(AtomicUsize);

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

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "multi_thread")]
async fn one_producer_many_consumers_each_message_once() {
    let numbers = Channel::<u64>::bounded(4);
    let seen = Actor::new(Vec::new());
    let result = Tasks::<&str>::run(async |t| {
        let producer = numbers.clone();
        t.spawn(async move {
            for n in 0..100 {
                producer.send(n).await.map_err(|_| "closed early")?;
            }
            assert!(producer.close());
            Ok(())
        });
        for _ in 0..3 {
            let consumer = numbers.clone();
            let seen = seen.clone();
            t.spawn(async move {
                while let Some(n) = consumer.receive().await {
                    seen.with(|seen| seen.push(n));
                }
                Ok(())
            });
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
    let mut seen = seen.snapshot();
    seen.sort_unstable();
    assert_eq!(seen, (0..100).collect::<Vec<_>>());
    assert!(numbers.is_closed());
}

#[test]
fn a_closed_channel_drains_then_ends() {
    let counter = Arc::new(WakeCounter::default());
    let ch = Channel::bounded(8);
    assert_eq!(poll_once(pin!(ch.send(1)), &counter), Poll::Ready(Ok(())));
    assert_eq!(poll_once(pin!(ch.send(2)), &counter), Poll::Ready(Ok(())));
    assert_eq!(ch.len(), 2);
    assert!(ch.close());
    assert!(!ch.close());
    assert_eq!(
        poll_once(pin!(ch.send(3)), &counter),
        Poll::Ready(Err(Closed(3)))
    );
    assert_eq!(
        poll_once(pin!(ch.receive()), &counter),
        Poll::Ready(Some(1))
    );
    assert_eq!(ch.try_receive(), Ok(2));
    assert_eq!(poll_once(pin!(ch.receive()), &counter), Poll::Ready(None));
    assert_eq!(ch.try_receive(), Err(TryReceiveError::Closed));
    assert!(ch.is_empty());
}

#[test]
fn zero_capacity_is_one() {
    let ch = Channel::<u8>::bounded(0);
    assert_eq!(ch.capacity(), 1);
    assert_eq!(
        format!("{ch:?}"),
        "Channel { len: 0, capacity: 1, closed: false }"
    );
}

#[test]
fn nonblocking_operations_distinguish_full_empty_and_closed() {
    let combined = Channel::bounded(1);
    assert_eq!(combined.try_send(String::from("first")), Ok(()));
    assert_eq!(
        combined.try_send(String::from("full")),
        Err(TrySendError::Full(String::from("full")))
    );
    let (sender, receiver) = combined.split();
    assert_eq!(receiver.try_receive(), Ok(String::from("first")));
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(sender.try_send(String::from("queued")), Ok(()));
    assert_eq!(
        sender.try_send(String::from("full")),
        Err(TrySendError::Full(String::from("full")))
    );
    assert!(sender.close());
    assert_eq!(
        sender.try_send(String::from("closed")),
        Err(TrySendError::Closed(String::from("closed")))
    );
    assert_eq!(receiver.try_receive(), Ok(String::from("queued")));
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));

    let combined = Channel::bounded(1);
    assert_eq!(combined.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(combined.try_send(1), Ok(()));
    assert_eq!(combined.try_receive(), Ok(1));
    assert_eq!(combined.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(combined.try_send(2), Ok(()));
    assert!(combined.close());
    assert_eq!(combined.try_send(3), Err(TrySendError::Closed(3)));
    assert_eq!(combined.try_receive(), Ok(2));
    assert_eq!(combined.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn dropping_last_sender_drains_queued_values() {
    let (sender, receiver) = Channel::bounded(2).split();
    let other = sender.clone();
    sender.try_send(1).unwrap();
    other.try_send(2).unwrap();
    drop(sender);
    assert!(!receiver.is_closed());
    assert_eq!(receiver.try_receive(), Ok(1));
    drop(other);
    assert!(receiver.is_closed());
    assert_eq!(receiver.try_receive(), Ok(2));
    let counter = Arc::new(WakeCounter::default());
    assert_eq!(
        poll_once(pin!(receiver.receive()), &counter),
        Poll::Ready(None)
    );
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn dropping_last_sender_wakes_all_pending_receivers() {
    let (sender, receiver) = Channel::<u8>::bounded(1).split();
    let other_sender = sender.clone();
    let other_receiver = receiver.clone();
    let first_wake = Arc::new(WakeCounter::default());
    let second_wake = Arc::new(WakeCounter::default());
    let mut first = pin!(receiver.receive());
    let mut second = pin!(other_receiver.receive());
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());
    drop(sender);
    assert!(!receiver.is_closed());
    assert_eq!(first_wake.0.load(Ordering::SeqCst), 0);
    assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
    drop(other_sender);
    assert!(first_wake.0.load(Ordering::SeqCst) > 0);
    assert!(second_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(poll_once(first.as_mut(), &first_wake), Poll::Ready(None));
    assert_eq!(poll_once(second.as_mut(), &second_wake), Poll::Ready(None));
}

#[test]
fn dropping_last_receiver_wakes_all_pending_senders() {
    let (sender, receiver) = Channel::bounded(1).split();
    let other_sender = sender.clone();
    let other_receiver = receiver.clone();
    sender.try_send(1).unwrap();
    let first_wake = Arc::new(WakeCounter::default());
    let second_wake = Arc::new(WakeCounter::default());
    let mut first = pin!(sender.send(2));
    let mut second = pin!(other_sender.send(3));
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());
    drop(receiver);
    assert!(!sender.is_closed());
    assert_eq!(first_wake.0.load(Ordering::SeqCst), 0);
    assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
    drop(other_receiver);
    assert!(first_wake.0.load(Ordering::SeqCst) > 0);
    assert!(second_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        poll_once(first.as_mut(), &first_wake),
        Poll::Ready(Err(Closed(2)))
    );
    assert_eq!(
        poll_once(second.as_mut(), &second_wake),
        Poll::Ready(Err(Closed(3)))
    );
    assert_eq!(sender.try_send(4), Err(TrySendError::Closed(4)));
}

#[test]
fn blocked_operations_resume_when_space_or_data_arrives() {
    let (sender, receiver) = Channel::bounded(1).split();
    let counter = Arc::new(WakeCounter::default());
    let mut receive = pin!(receiver.receive());
    assert!(poll_once(receive.as_mut(), &counter).is_pending());
    sender.try_send(1).unwrap();
    assert!(counter.0.swap(0, Ordering::SeqCst) > 0);
    assert_eq!(poll_once(receive.as_mut(), &counter), Poll::Ready(Some(1)));

    sender.try_send(2).unwrap();
    let mut send = pin!(sender.send(3));
    assert!(poll_once(send.as_mut(), &counter).is_pending());
    assert_eq!(receiver.try_receive(), Ok(2));
    assert!(counter.0.load(Ordering::SeqCst) > 0);
    assert_eq!(poll_once(send.as_mut(), &counter), Poll::Ready(Ok(())));
    assert_eq!(receiver.try_receive(), Ok(3));
}

#[test]
fn explicit_close_from_either_endpoint_wakes_pending_operations() {
    for close_from_sender in [true, false] {
        let (sender, receiver) = Channel::<u8>::bounded(1).split();
        let other = receiver.clone();
        let first_wake = Arc::new(WakeCounter::default());
        let second_wake = Arc::new(WakeCounter::default());
        let mut first = pin!(receiver.receive());
        let mut second = pin!(other.receive());
        assert!(poll_once(first.as_mut(), &first_wake).is_pending());
        assert!(poll_once(second.as_mut(), &second_wake).is_pending());
        assert!(if close_from_sender {
            sender.close()
        } else {
            receiver.close()
        });
        assert!(!sender.close());
        assert!(!receiver.close());
        assert!(first_wake.0.load(Ordering::SeqCst) > 0);
        assert!(second_wake.0.load(Ordering::SeqCst) > 0);
        assert_eq!(poll_once(first.as_mut(), &first_wake), Poll::Ready(None));
        assert_eq!(poll_once(second.as_mut(), &second_wake), Poll::Ready(None));

        let (sender, receiver) = Channel::bounded(1).split();
        sender.try_send(1).unwrap();
        let counter = Arc::new(WakeCounter::default());
        let mut send = pin!(sender.send(2));
        assert!(poll_once(send.as_mut(), &counter).is_pending());
        assert!(if close_from_sender {
            sender.close()
        } else {
            receiver.close()
        });
        assert!(counter.0.load(Ordering::SeqCst) > 0);
        assert_eq!(
            poll_once(send.as_mut(), &counter),
            Poll::Ready(Err(Closed(2)))
        );
        assert_eq!(receiver.try_receive(), Ok(1));
        assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));
    }
}

#[test]
fn retained_combined_handles_own_both_endpoints() {
    let combined = Channel::bounded(1);
    let retained = combined.clone();
    let (sender, receiver) = combined.split();
    drop(sender);
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Empty));
    retained.try_send(1).unwrap();
    assert_eq!(receiver.try_receive(), Ok(1));
    drop(retained);
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));

    let combined = Channel::bounded(1);
    let retained = combined.clone();
    let (sender, receiver) = combined.split();
    drop(receiver);
    assert_eq!(sender.try_send(1), Ok(()));
    let counter = Arc::new(WakeCounter::default());
    assert_eq!(
        poll_once(pin!(retained.receive()), &counter),
        Poll::Ready(Some(1))
    );
    drop(retained);
    assert_eq!(sender.try_send(2), Err(TrySendError::Closed(2)));
}

#[test]
fn multiple_producers_and_consumers_deliver_each_message_once_without_a_runtime() {
    let (sender, receiver) = Channel::bounded(2).split();
    let mut producers: Vec<_> = (0..4)
        .map(|producer| {
            let sender = sender.clone();
            Some(Box::pin(async move {
                for item in 0..25 {
                    sender.send(producer * 25 + item).await.unwrap();
                }
            }))
        })
        .collect();
    let mut consumers: Vec<_> = (0..3)
        .map(|_| {
            let receiver = receiver.clone();
            Some(Box::pin(async move {
                let mut seen = Vec::new();
                while let Some(value) = receiver.receive().await {
                    seen.push(value);
                }
                seen
            }))
        })
        .collect();
    drop(sender);
    drop(receiver);
    let counter = Arc::new(WakeCounter::default());
    let mut seen = Vec::new();
    for _ in 0..1000 {
        for producer in &mut producers {
            if let Some(future) = producer
                && poll_once(future.as_mut(), &counter).is_ready()
            {
                *producer = None;
            }
        }
        for consumer in &mut consumers {
            if let Some(future) = consumer
                && let Poll::Ready(values) = poll_once(future.as_mut(), &counter)
            {
                seen.extend(values);
                *consumer = None;
            }
        }
        if consumers.iter().all(Option::is_none) {
            break;
        }
    }
    assert!(producers.iter().all(Option::is_none));
    assert!(consumers.iter().all(Option::is_none));
    seen.sort_unstable();
    assert_eq!(seen, (0..100).collect::<Vec<_>>());
}

#[test]
fn endpoints_clone_without_cloning_messages_and_report_queue_state() {
    struct Message;
    let (sender, receiver) = Channel::<Message>::bounded(0).split();
    let other_sender = sender.clone();
    let other_receiver = receiver.clone();
    assert_eq!(sender.capacity(), 1);
    assert_eq!(receiver.capacity(), 1);
    assert!(sender.is_empty());
    assert!(receiver.is_empty());
    assert!(other_sender.try_send(Message).is_ok());
    assert_eq!(sender.len(), 1);
    assert_eq!(receiver.len(), 1);
    assert!(!sender.is_empty());
    assert!(!receiver.is_empty());
    assert_eq!(
        format!("{sender:?}"),
        "Sender { len: 1, capacity: 1, closed: false }"
    );
    assert_eq!(
        format!("{receiver:?}"),
        "Receiver { len: 1, capacity: 1, closed: false }"
    );
    assert!(other_receiver.try_receive().is_ok());
    assert!(other_receiver.close());
    assert!(other_sender.is_closed());
}

#[cfg(feature = "tokio")]
mod stream {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn drains_queued_messages_before_ending() {
        struct Message(u8, std::marker::PhantomPinned);

        for explicitly_close in [true, false] {
            let (sender, receiver) = Channel::bounded(2).split();
            assert!(
                sender
                    .try_send(Message(1, std::marker::PhantomPinned))
                    .is_ok()
            );
            assert!(
                sender
                    .try_send(Message(2, std::marker::PhantomPinned))
                    .is_ok()
            );
            if explicitly_close {
                assert!(sender.close());
            } else {
                drop(sender);
            }

            let values: Vec<_> = receiver.map(|message| message.0).collect().await;
            assert_eq!(values, [1, 2]);
        }
    }

    #[test]
    fn wakes_on_send_after_a_pending_next_is_dropped() {
        let (sender, receiver) = Channel::bounded(1).split();
        let counter = Arc::new(WakeCounter::default());
        let mut receiver = pin!(receiver);

        for value in 1..=2 {
            assert!(poll_once(pin!(receiver.next()), &counter).is_pending());
            assert_eq!(counter.0.swap(0, Ordering::SeqCst), 0);
            sender.try_send(value).unwrap();
            assert!(counter.0.swap(0, Ordering::SeqCst) > 0);
            assert_eq!(
                poll_once(pin!(receiver.next()), &counter),
                Poll::Ready(Some(value))
            );
        }
    }

    #[test]
    fn dropping_last_sender_wakes_pending_stream_clones() {
        let (sender, receiver) = Channel::<u8>::bounded(1).split();
        let other_sender = sender.clone();
        let other_receiver = receiver.clone();
        let first_wake = Arc::new(WakeCounter::default());
        let second_wake = Arc::new(WakeCounter::default());
        let mut first = pin!(receiver);
        let mut second = pin!(other_receiver);

        assert!(poll_once(pin!(first.next()), &first_wake).is_pending());
        assert!(poll_once(pin!(second.next()), &second_wake).is_pending());
        drop(sender);
        assert_eq!(first_wake.0.load(Ordering::SeqCst), 0);
        assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
        drop(other_sender);
        assert!(first_wake.0.load(Ordering::SeqCst) > 0);
        assert!(second_wake.0.load(Ordering::SeqCst) > 0);
        assert_eq!(
            poll_once(pin!(first.next()), &first_wake),
            Poll::Ready(None)
        );
        assert_eq!(
            poll_once(pin!(second.next()), &second_wake),
            Poll::Ready(None)
        );
    }

    #[tokio::test]
    async fn receiving_from_a_stream_wakes_pending_send_until() {
        use adamantium::Cancellation;

        let (sender, receiver) = Channel::bounded(1).split();
        let cancellation = Cancellation::new();
        let send_wake = Arc::new(WakeCounter::default());
        let receive_wake = Arc::new(WakeCounter::default());
        let mut receiver = pin!(receiver);
        sender.try_send(0).unwrap();

        for value in 1..=2 {
            let mut send = pin!(sender.send_until(&cancellation, None, value));
            assert!(poll_once(send.as_mut(), &send_wake).is_pending());
            assert_eq!(
                poll_once(pin!(receiver.next()), &receive_wake),
                Poll::Ready(Some(value - 1))
            );
            assert!(send_wake.0.swap(0, Ordering::SeqCst) > 0);
            assert_eq!(poll_once(send.as_mut(), &send_wake), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn explicit_close_wakes_a_pending_stream() {
        for close_from_sender in [true, false] {
            let (sender, receiver) = Channel::<u8>::bounded(1).split();
            let counter = Arc::new(WakeCounter::default());
            let mut receiver = pin!(receiver);
            assert!(poll_once(pin!(receiver.next()), &counter).is_pending());
            assert!(if close_from_sender {
                sender.close()
            } else {
                receiver.close()
            });
            assert!(counter.0.load(Ordering::SeqCst) > 0);
            assert_eq!(
                poll_once(pin!(receiver.next()), &counter),
                Poll::Ready(None)
            );
            assert_eq!(
                poll_once(pin!(receiver.next()), &counter),
                Poll::Ready(None)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn composes_with_concurrent_results_under_backpressure() {
        use adamantium::concurrently_ordered;
        use std::time::Duration;

        let result = crate::support::timeout(
            Duration::from_secs(30),
            Tasks::<&str>::run(async |tasks| {
                let (sender, receiver) = Channel::bounded(1).split();
                tasks.spawn(async move {
                    for value in 0..20 {
                        sender.send(value).await.map_err(|_| "closed early")?;
                    }
                    Ok(())
                });
                let results =
                    concurrently_ordered(0..20, 3, |value| async move { Ok::<_, &str>(value * 2) });
                let values: Vec<_> = receiver.zip(results).collect().await;
                assert_eq!(
                    values,
                    (0..20)
                        .map(|value| (value, Ok(value * 2)))
                        .collect::<Vec<_>>()
                );
                Ok(())
            }),
        )
        .await
        .unwrap();
        assert_eq!(result, Ok(()));
    }
}

#[test]
fn dropping_a_notified_receive_passes_the_message_to_the_next_waiter() {
    let (sender, receiver) = Channel::bounded(1).split();
    let first_wake = Arc::new(WakeCounter::default());
    let second_wake = Arc::new(WakeCounter::default());
    let mut first = Box::pin(receiver.receive());
    let mut second = pin!(receiver.receive());
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());
    sender.try_send(7).unwrap();
    assert!(first_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
    drop(first);
    assert!(second_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        poll_once(second.as_mut(), &second_wake),
        Poll::Ready(Some(7))
    );
}

#[test]
fn dropping_a_notified_send_passes_room_without_sending_its_value() {
    let (sender, receiver) = Channel::bounded(1).split();
    sender.try_send(0).unwrap();
    let first_wake = Arc::new(WakeCounter::default());
    let second_wake = Arc::new(WakeCounter::default());
    let mut first = Box::pin(sender.send(1));
    let mut second = pin!(sender.send(2));
    assert!(poll_once(first.as_mut(), &first_wake).is_pending());
    assert!(poll_once(second.as_mut(), &second_wake).is_pending());
    assert_eq!(receiver.try_receive(), Ok(0));
    assert!(first_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
    drop(first);
    assert!(second_wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        poll_once(second.as_mut(), &second_wake),
        Poll::Ready(Ok(()))
    );
    assert_eq!(receiver.try_receive(), Ok(2));
    assert_eq!(receiver.try_receive(), Err(TryReceiveError::Empty));
}
