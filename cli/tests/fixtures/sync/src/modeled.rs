use adamantium::{Cancellation, Channel, Limit, Once, Reply, Topic, Watch, concurrently};

pub async fn queues_and_replies() -> Option<u8> {
    let (sender, receiver) = Channel::bounded(1).split();
    sender.send(2_u8).await.ok()?;
    let (reply, answer) = Reply::pair();
    reply.send(receiver.receive().await?).ok()?;
    answer.receive().await
}

pub async fn state_and_broadcast() -> Option<(u8, u8)> {
    let watch = Watch::new(0_u8);
    let mut watcher = watch.subscribe();
    watch.send(1).ok()?;

    let mut topic = Topic::new(1);
    let subscriber = topic.subscribe();
    assert_eq!(topic.send(2_u8).await, 1);
    Some((watcher.receive().await?, subscriber.receive().await?))
}

pub async fn signals_and_limits() -> Result<Vec<()>, std::convert::Infallible> {
    let shutdown = Cancellation::new();
    shutdown.cancel();
    shutdown.cancelled().await;

    let (signal, notified) = Channel::bounded(1).split();
    assert!(signal.try_send(()).is_ok());
    assert_eq!(notified.receive().await, Some(()));

    let limit = Limit::new(2);
    limit.run(|| async {}).await;
    concurrently([()], 1, |()| async { Ok(()) }).await
}

pub async fn cells() -> u8 {
    let connection = Once::new();
    *connection.get_or_init(|| async { 1 }).await
}
