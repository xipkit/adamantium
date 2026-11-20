use adamantium::{Topic, TryReceiveError};
use std::{
    future::Future,
    pin::{Pin, pin},
    task::{Context, Poll, Waker},
};

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn send_now<T: Clone>(topic: &mut Topic<T>, value: T) -> usize {
    let mut send = pin!(topic.send(value));
    match poll_once(send.as_mut()) {
        Poll::Ready(delivered) => delivered,
        Poll::Pending => panic!("the send waited"),
    }
}

#[test]
fn every_subscriber_receives_every_message_in_order() {
    let mut topic = Topic::new(4);
    let first = topic.subscribe();
    let second = topic.subscribe();
    for n in 1..=3 {
        assert_eq!(send_now(&mut topic, n), 2);
    }
    for subscriber in [&first, &second] {
        assert_eq!(subscriber.try_receive(), Ok(1));
        assert_eq!(subscriber.try_receive(), Ok(2));
        assert_eq!(subscriber.try_receive(), Ok(3));
        assert_eq!(subscriber.try_receive(), Err(TryReceiveError::Empty));
    }
}

#[test]
fn a_subscriber_receives_only_what_is_sent_after_it_subscribes() {
    let mut topic = Topic::new(4);
    let early = topic.subscribe();
    assert_eq!(send_now(&mut topic, "before"), 1);
    let late = topic.subscribe();
    assert_eq!(send_now(&mut topic, "after"), 2);
    assert_eq!(early.try_receive(), Ok("before"));
    assert_eq!(early.try_receive(), Ok("after"));
    assert_eq!(late.try_receive(), Ok("after"));
}

#[test]
fn a_send_with_no_subscribers_delivers_to_nobody() {
    let mut topic = Topic::new(1);
    assert_eq!(send_now(&mut topic, 1), 0);
    assert_eq!(topic.subscriber_count(), 0);
}

#[test]
fn a_full_subscriber_holds_the_message_back_from_everyone() {
    let mut topic = Topic::new(1);
    let slow = topic.subscribe();
    let fast = topic.subscribe();
    assert_eq!(send_now(&mut topic, 1), 2);
    assert_eq!(fast.try_receive(), Ok(1));
    {
        let mut send = pin!(topic.send(2));
        assert_eq!(poll_once(send.as_mut()), Poll::Pending);
        assert_eq!(fast.try_receive(), Err(TryReceiveError::Empty));

        assert_eq!(slow.try_receive(), Ok(1));
        assert_eq!(poll_once(send.as_mut()), Poll::Ready(2));
    }
    assert_eq!(slow.try_receive(), Ok(2));
    assert_eq!(fast.try_receive(), Ok(2));
}

#[test]
fn a_send_dropped_while_it_waits_delivers_to_nobody() {
    let mut topic = Topic::new(1);
    let slow = topic.subscribe();
    let fast = topic.subscribe();
    assert_eq!(send_now(&mut topic, 1), 2);
    assert_eq!(fast.try_receive(), Ok(1));
    {
        let mut send = pin!(topic.send(2));
        assert_eq!(poll_once(send.as_mut()), Poll::Pending);
    }
    assert_eq!(fast.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(slow.try_receive(), Ok(1));
    assert_eq!(slow.try_receive(), Err(TryReceiveError::Empty));
}

#[test]
fn dropping_a_receiver_unsubscribes_it() {
    let mut topic = Topic::new(1);
    let stays = topic.subscribe();
    let leaves = topic.subscribe();
    assert_eq!(topic.subscriber_count(), 2);
    drop(leaves);
    assert_eq!(topic.subscriber_count(), 1);
    assert_eq!(send_now(&mut topic, 1), 1);
    assert_eq!(stays.try_receive(), Ok(1));
}

#[test]
fn a_full_subscriber_that_leaves_stops_holding_the_topic_back() {
    let mut topic = Topic::new(1);
    let full = topic.subscribe();
    let other = topic.subscribe();
    assert_eq!(send_now(&mut topic, 1), 2);
    assert_eq!(other.try_receive(), Ok(1));
    let mut send = pin!(topic.send(2));
    assert_eq!(poll_once(send.as_mut()), Poll::Pending);
    drop(full);
    assert_eq!(poll_once(send.as_mut()), Poll::Ready(1));
    assert_eq!(other.try_receive(), Ok(2));
}

#[test]
fn dropping_the_topic_closes_subscribers_after_they_drain() {
    let mut topic = Topic::new(2);
    let subscriber = topic.subscribe();
    assert_eq!(send_now(&mut topic, 1), 1);
    drop(topic);
    assert_eq!(subscriber.try_receive(), Ok(1));
    assert_eq!(subscriber.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn subscriptions_add_subscribers_from_a_clone() {
    let mut topic = Topic::new(2);
    let subscriptions = topic.subscriptions();
    let subscriber = subscriptions.clone().subscribe();
    assert_eq!(send_now(&mut topic, 1), 1);
    assert_eq!(subscriber.try_receive(), Ok(1));

    drop(topic);
    let late = subscriptions.subscribe();
    assert_eq!(late.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn the_message_is_cloned_once_for_each_subscriber_but_the_last() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Counted(Arc<AtomicUsize>);
    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::Relaxed);
            Self(Arc::clone(&self.0))
        }
    }

    let clones = Arc::new(AtomicUsize::new(0));
    let mut topic = Topic::new(1);
    let _subscribers = [topic.subscribe(), topic.subscribe(), topic.subscribe()];
    assert_eq!(send_now(&mut topic, Counted(Arc::clone(&clones))), 3);
    assert_eq!(clones.load(Ordering::Relaxed), 2);
}

struct Message {
    drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    resubscribe: Option<adamantium::Subscriptions<Message>>,
}

impl Clone for Message {
    fn clone(&self) -> Self {
        Self {
            drops: std::sync::Arc::clone(&self.drops),
            resubscribe: self.resubscribe.clone(),
        }
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        self.drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(subscriptions) = &self.resubscribe {
            drop(subscriptions.subscribe());
        }
    }
}

#[test]
fn a_subscriber_that_leaves_frees_its_queue() {
    use std::sync::atomic::Ordering;

    let drops = std::sync::Arc::default();
    let mut topic = Topic::new(2);
    let leaves = topic.subscribe();
    let message = Message {
        drops: std::sync::Arc::clone(&drops),
        resubscribe: None,
    };
    assert_eq!(send_now(&mut topic, message), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(leaves);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(topic.subscriber_count(), 0);
}

#[test]
fn a_message_that_subscribes_when_dropped_does_not_deadlock() {
    use std::sync::atomic::Ordering;

    let drops = std::sync::Arc::default();
    let mut topic = Topic::new(2);
    let leaves = topic.subscribe();
    let message = Message {
        drops: std::sync::Arc::clone(&drops),
        resubscribe: Some(topic.subscriptions()),
    };
    assert_eq!(send_now(&mut topic, message), 1);
    drop(leaves);

    assert_eq!(topic.subscriber_count(), 0);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[cfg(feature = "tokio")]
mod until {
    use super::*;
    use adamantium::{Cancellation, SendError, Tasks, time};
    use std::time::Duration;

    #[tokio::test]
    async fn cancellation_returns_the_value_and_delivers_to_nobody() {
        let mut topic = Topic::new(1);
        let slow = topic.subscribe();
        let fast = topic.subscribe();
        assert_eq!(topic.send(1).await, 2);
        assert_eq!(fast.try_receive(), Ok(1));

        let cancellation = Cancellation::new();
        cancellation.cancel();
        let result = topic.send_until(&cancellation, None, 2).await;
        assert_eq!(result, Err(SendError::Cancelled(2)));
        assert_eq!(fast.try_receive(), Err(TryReceiveError::Empty));
        assert_eq!(slow.try_receive(), Ok(1));
    }

    #[tokio::test]
    async fn a_request_already_present_wins_over_room() {
        let mut topic = Topic::new(1);
        let subscriber = topic.subscribe();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let result = topic.send_until(&cancellation, None, 1).await;
        assert_eq!(result, Err(SendError::Cancelled(1)));
        assert_eq!(subscriber.try_receive(), Err(TryReceiveError::Empty));
    }

    #[tokio::test]
    async fn a_request_already_present_wins_with_no_subscribers() {
        let mut topic = Topic::new(1);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let result = topic.send_until(&cancellation, None, 1).await;
        assert_eq!(result, Err(SendError::Cancelled(1)));

        let expired = time::now();
        let result = topic
            .send_until(&Cancellation::new(), Some(expired), 2)
            .await;
        assert_eq!(result, Err(SendError::DeadlineExceeded(2)));
    }

    #[tokio::test]
    async fn the_deadline_returns_the_value() {
        let mut topic = Topic::new(1);
        let _slow = topic.subscribe();
        assert_eq!(topic.send(1).await, 1);
        let deadline = time::now() + Duration::from_millis(20);
        let result = topic
            .send_until(&Cancellation::new(), Some(deadline), 2)
            .await;
        assert_eq!(result, Err(SendError::DeadlineExceeded(2)));
    }

    #[tokio::test]
    async fn a_waiting_send_finishes_when_a_task_makes_room() {
        let mut topic = Topic::new(1);
        let subscriber = topic.subscribe();
        assert_eq!(topic.send(1).await, 1);
        let received = Tasks::<()>::run(async |tasks| {
            let reader = tasks.spawn(async move {
                let first = subscriber.receive().await;
                let second = subscriber.receive().await;
                Ok((first, second))
            });
            let delivered = topic.send_until(&Cancellation::new(), None, 2).await;
            assert_eq!(delivered, Ok(1));
            drop(topic);
            tasks.value(reader).await
        })
        .await;
        assert_eq!(received, Ok((Some(1), Some(2))));
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn every_schedule_delivers_every_message_to_every_subscriber_in_order() {
    use adamantium::testing::{self, Config};

    type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

    let config = Config {
        seed: 181,
        iterations: 300,
        max_steps: 20_000,
        ..Config::default()
    };
    let checked = testing::check(config, || async {
        let mut topic = Topic::new(1);
        let mut workers: Vec<Worker> = Vec::new();
        for _ in 0..2 {
            let subscriber = topic.subscribe();
            workers.push(Box::pin(async move {
                for expected in 0..4 {
                    assert_eq!(subscriber.receive().await, Some(expected));
                }
                assert_eq!(subscriber.receive().await, None);
            }));
        }
        workers.push(Box::pin(async move {
            for message in 0..4 {
                assert_eq!(topic.send(message).await, 2);
            }
        }));
        testing::parallel(workers).await;
    });
    assert!(checked.is_ok(), "{checked:?}");
}
