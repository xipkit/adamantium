#[cfg(feature = "tokio")]
use adamantium::{Actor, Tasks};
use adamantium::{Channel, Closed};

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

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_channel_drains_then_ends() {
    let ch = Channel::bounded(8);
    ch.send(1).await.unwrap();
    ch.send(2).await.unwrap();
    assert_eq!(ch.len(), 2);
    assert!(ch.close());
    assert!(!ch.close());
    assert_eq!(ch.send(3).await, Err(Closed(3)));
    assert_eq!(ch.receive().await, Some(1));
    assert_eq!(ch.try_receive(), Some(2));
    assert_eq!(ch.receive().await, None);
    assert_eq!(ch.try_receive(), None);
    assert!(ch.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_capacity_is_one() {
    let ch = Channel::<u8>::bounded(0);
    assert_eq!(ch.capacity(), 1);
    assert_eq!(
        format!("{ch:?}"),
        "Channel { len: 0, capacity: 1, closed: false }"
    );
}
