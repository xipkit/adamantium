#![allow(dead_code)]

use tokio::sync::broadcast::Receiver as Subscription; // disallowed_types
use tokio::sync::broadcast::channel as broadcast;
use tokio::sync::{Semaphore, watch};

fn broadcasts() {
    let (sender, receiver) = tokio::sync::broadcast::channel::<u8>(1); // disallowed_methods
    let (other, _subscription) = broadcast::<u8>(1); // disallowed_methods
    publish(&sender, receiver);
    drop(other);
}

fn publish(
    sender: &tokio::sync::broadcast::Sender<u8>, // disallowed_types
    receiver: Subscription<u8>,                  // disallowed_types
) {
    drop(sender.send(1));
    drop(receiver);
}

async fn gate(semaphore: &Semaphore) {
    let permit = semaphore.acquire().await;
    std::future::ready(()).await;
    drop(permit);
}

fn consume(semaphore: &Semaphore) {
    if let Ok(permit) = semaphore.try_acquire() {
        permit.forget();
    }
}

fn latest() {
    let (sender, receiver) = watch::channel(0_u8);
    drop(sender.send(1));
    drop(receiver);
}

async fn fan_out(subscribers: &[adamantium::Channel<u8>]) {
    for subscriber in subscribers {
        drop(subscriber.send(1).await);
    }
}
