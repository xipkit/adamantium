#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    reason = "benchmark exercises the production executor with a fixed workload"
)]

use std::time::Instant;

use adamantium::Channel;

fn main() {
    let capacity = std::env::args().nth(1).map_or(64, |v| v.parse().unwrap());
    let count = std::env::args()
        .nth(2)
        .map_or(100_000, |v| v.parse().unwrap());
    let producers = std::env::args().nth(3).map_or(4, |v| v.parse().unwrap());
    let receivers = std::env::args().nth(4).map_or(4, |v| v.parse().unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .unwrap();
    let start = Instant::now();
    runtime.block_on(async {
        let (sender, receiver) = Channel::bounded(capacity).split();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(
            (producers + receivers).try_into().unwrap(),
        ));
        let mut producer_tasks = Vec::new();
        let mut consumers = Vec::new();
        for producer in 0..producers {
            let sender = sender.clone();
            let barrier = barrier.clone();
            producer_tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                for item in 0..count {
                    sender.send(producer * count + item).await.unwrap();
                }
            }));
        }
        for _ in 0..receivers {
            let receiver = receiver.clone();
            let barrier = barrier.clone();
            consumers.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut total = 0_u64;
                let mut seen = 0;
                while let Some(value) = receiver.receive().await {
                    total += value;
                    seen += 1;
                }
                (seen, total)
            }));
        }
        drop(sender);
        drop(receiver);
        for producer in producer_tasks {
            producer.await.unwrap();
        }
        let mut seen = 0;
        let mut total = 0;
        for consumer in consumers {
            let (n, sum) = consumer.await.unwrap();
            seen += n;
            total += sum;
        }
        assert_eq!(seen, producers * count);
        assert_eq!(total, (producers * count) * (producers * count - 1) / 2);
    });
    println!("{}", start.elapsed().as_nanos());
}
