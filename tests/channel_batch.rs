use adamantium::{Channel, TryReceiveError};

#[test]
fn try_receive_batch_respects_the_limit_and_drains_after_close() {
    let channel = Channel::<u8>::bounded(3);
    assert_eq!(channel.try_receive_batch(2), Err(TryReceiveError::Empty));
    for value in 1..=3 {
        channel.try_send(value).unwrap();
    }
    assert_eq!(channel.try_receive_batch(0), Ok(vec![]));
    assert_eq!(channel.try_receive_batch(2), Ok(vec![1, 2]));

    let (sender, receiver) = channel.split();
    drop(sender);
    assert_eq!(receiver.try_receive_batch(usize::MAX), Ok(vec![3]));
    assert_eq!(receiver.try_receive_batch(2), Err(TryReceiveError::Closed));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn a_zero_limit_leaves_the_queue_alone_and_zero_linger_takes_one_value() {
    use adamantium::Cancellation;
    use std::time::Duration;

    let channel = Channel::bounded(2);
    channel.try_send(1).unwrap();
    channel.try_send(2).unwrap();
    let cancellation = Cancellation::new();
    let mut batch = vec![0];
    assert_eq!(
        channel
            .receive_batch_until(&cancellation, 0, Duration::ZERO, &mut batch)
            .await,
        Ok([].as_slice())
    );
    assert_eq!(batch, [0]);
    assert_eq!(
        channel
            .receive_batch_until(&cancellation, 2, Duration::ZERO, &mut batch)
            .await,
        Ok([1].as_slice())
    );
    assert_eq!(batch, [0, 1]);
    assert_eq!(channel.try_receive(), Ok(2));
}

#[cfg(feature = "deterministic")]
mod modeled {
    use std::{pin::pin, time::Duration};

    use adamantium::{Cancellation, Channel, ReceiveError, testing, time};
    use futures_util::poll;

    const LINGER: Duration = Duration::from_secs(10);

    #[test]
    fn a_batch_can_fill_before_the_linger_with_a_smaller_queue() {
        testing::check(testing::Config::default(), || async {
            let (sender, receiver) = Channel::bounded(1).split();
            let cancellation = Cancellation::new();
            let mut batch = vec![0];
            let start = time::now();
            {
                let mut receive =
                    pin!(receiver.receive_batch_until(&cancellation, 3, LINGER, &mut batch));
                for value in 1..=2 {
                    sender.try_send(value).unwrap();
                    assert!(poll!(&mut receive).is_pending());
                }
                sender.try_send(3).unwrap();
                assert_eq!(receive.await, Ok([1, 2, 3].as_slice()));
            }
            assert_eq!(batch, [0, 1, 2, 3]);
            assert_eq!(time::now(), start);
        })
        .unwrap();
    }

    #[test]
    fn linger_starts_at_the_first_value_and_returns_the_partial_batch() {
        testing::check(testing::Config::default(), || async {
            let channel = Channel::bounded(1);
            let cancellation = Cancellation::new();
            let mut batch = Vec::new();
            let mut receive =
                pin!(channel.receive_batch_until(&cancellation, 3, LINGER, &mut batch));
            assert!(poll!(&mut receive).is_pending());
            time::sleep(LINGER * 2).await;
            assert!(poll!(&mut receive).is_pending());

            let first = time::now();
            channel.try_send(1).unwrap();
            assert!(poll!(&mut receive).is_pending());
            time::sleep(LINGER / 2).await;
            channel.try_send(2).unwrap();
            assert!(poll!(&mut receive).is_pending());

            assert_eq!(receive.await, Ok([1, 2].as_slice()));
            assert_eq!(time::now(), first + LINGER);
        })
        .unwrap();
    }

    #[test]
    fn closing_mid_batch_drains_the_queue_before_reporting_close() {
        testing::check(testing::Config::default(), || async {
            let (sender, receiver) = Channel::bounded(1).split();
            let cancellation = Cancellation::new();
            let mut batch = Vec::new();
            let start = time::now();
            sender.try_send(1).unwrap();
            {
                let mut receive =
                    pin!(receiver.receive_batch_until(&cancellation, 3, LINGER, &mut batch));
                assert!(poll!(&mut receive).is_pending());
                sender.try_send(2).unwrap();
                drop(sender);
                assert_eq!(receive.await, Ok([1, 2].as_slice()));
            }
            assert_eq!(time::now(), start);
            assert_eq!(
                receiver
                    .receive_batch_until(&cancellation, 3, LINGER, &mut batch)
                    .await,
                Err(ReceiveError::Closed)
            );
        })
        .unwrap();
    }

    #[test]
    fn cancellation_returns_held_values_and_leaves_the_rest_queued() {
        testing::check(testing::Config::default(), || async {
            let channel = Channel::bounded(1);
            let cancellation = Cancellation::new();
            let mut batch = Vec::new();
            channel.try_send(1).unwrap();
            {
                let mut receive =
                    pin!(channel.receive_batch_until(&cancellation, 3, LINGER, &mut batch));
                assert!(poll!(&mut receive).is_pending());
                channel.try_send(2).unwrap();
                cancellation.cancel();
                assert_eq!(receive.await, Ok([1].as_slice()));
            }
            assert_eq!(
                channel
                    .receive_batch_until(&cancellation, 3, LINGER, &mut batch)
                    .await,
                Err(ReceiveError::Cancelled)
            );
            assert_eq!(batch, [1]);
            assert_eq!(channel.try_receive(), Ok(2));
        })
        .unwrap();
    }

    #[test]
    fn dropping_a_batch_receive_leaves_held_values_in_the_buffer() {
        testing::check(testing::Config::default(), || async {
            let channel = Channel::bounded(1);
            let cancellation = Cancellation::new();
            let mut batch = Vec::new();
            channel.try_send(1).unwrap();
            {
                let mut receive =
                    pin!(channel.receive_batch_until(&cancellation, 3, LINGER, &mut batch));
                assert!(poll!(&mut receive).is_pending());
            }
            assert_eq!(batch, [1]);
            assert!(channel.is_empty());
        })
        .unwrap();
    }
}
