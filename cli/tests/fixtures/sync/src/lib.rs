#![allow(dead_code)]

mod modeled;

use std::sync::mpsc::sync_channel as blocking_channel;
use tokio::sync::Notify as Signal; // strict disallowed_types
use tokio::sync::OnceCell as Lazy; // strict disallowed_types
use tokio::sync::SetOnce as Published; // strict disallowed_types
use tokio::sync::broadcast::Receiver as Subscription; // disallowed_types
use tokio::sync::broadcast::channel as broadcast;
use tokio::sync::mpsc::channel as bounded;
use tokio::sync::oneshot::Sender as Answer; // disallowed_types
use tokio::sync::oneshot::channel as reply;
use tokio::sync::watch::Receiver as Observer; // disallowed_types
use tokio::sync::watch::channel as watch;

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

fn channels() {
    drop(tokio::sync::oneshot::channel::<u8>()); // disallowed_methods
    drop(reply::<u8>()); // disallowed_methods
    drop(tokio::sync::watch::channel(0_u8)); // disallowed_methods
    drop(watch(0_u8)); // disallowed_methods
    drop(tokio::sync::watch::Sender::new(0_u8)); // disallowed_types
    drop(std::sync::mpsc::sync_channel::<u8>(1)); // disallowed_methods
    drop(blocking_channel::<u8>(0)); // disallowed_methods
    drop(tokio::sync::mpsc::channel::<u8>(1)); // strict disallowed_methods
    drop(bounded::<u8>(1)); // strict disallowed_methods
}

fn endpoints(
    _reply: Answer<u8>,                          // disallowed_types
    _answer: tokio::sync::oneshot::Receiver<u8>, // disallowed_types
    _watch: tokio::sync::watch::Sender<u8>,      // disallowed_types
    _observer: Observer<u8>,                     // disallowed_types
    _sender: tokio::sync::mpsc::Sender<u8>,      // strict disallowed_types
    _receiver: tokio::sync::mpsc::Receiver<u8>,  // strict disallowed_types
) {
}

async fn gate(semaphore: &tokio::sync::Semaphore, // strict disallowed_types
) {
    let permit = semaphore.acquire().await;
    std::future::ready(()).await;
    drop(permit);
}

fn consume(semaphore: &tokio::sync::Semaphore, // strict disallowed_types
) {
    if let Ok(permit) = semaphore.try_acquire() {
        permit.forget();
    }
}

fn permits(
    _borrowed: tokio::sync::SemaphorePermit<'_>, // strict disallowed_types
    _owned: tokio::sync::OwnedSemaphorePermit,   // strict disallowed_types
) {
}

fn signals_and_limits(_notify: &Signal, // strict disallowed_types
) {
    drop(Signal::new()); // strict disallowed_types
    drop(Signal::const_new()); // strict disallowed_types
    drop(tokio::sync::Semaphore::new(2)); // strict disallowed_types
    drop(tokio::sync::Semaphore::const_new(2)); // strict disallowed_types
}

async fn cells(connection: &tokio::sync::OnceCell<u8>, // strict disallowed_types
) -> (u8, u8) {
    let keys: Lazy<u8> = Lazy::new(); // strict disallowed_types
    let first = *connection.get_or_init(|| async { 1 }).await;
    (first, *keys.get_or_init(|| async { 2 }).await)
}

static PORT: Published<u16> = Published::const_new(); // strict disallowed_types

async fn published(address: &tokio::sync::SetOnce<u16>, // strict disallowed_types
) -> u16 {
    drop(PORT.set(8080));
    *address.wait().await
}

fn blocking_boundary() {
    #[allow(
        clippy::disallowed_methods,
        reason = "the blocking caller requires a zero-capacity rendezvous"
    )]
    let (sender, receiver) = std::sync::mpsc::sync_channel::<u8>(0);
    drop((sender, receiver));
}
