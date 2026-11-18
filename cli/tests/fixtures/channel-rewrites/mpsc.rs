//! Bounded Tokio mpsc channels that `fix --strict` rewrites to split
//! `Channel`s. The program prints the same before and after.
use std::sync::Arc;
use tokio::sync::mpsc;

/// Values queued before the last sender drops are received, then nothing.
async fn drained() -> Vec<u32> {
    let (tx, mut rx) = mpsc::channel(4);
    for n in [1, 2, 3] {
        tx.send(n).await.unwrap_or(());
    }
    drop(tx);
    let mut received = Vec::new();
    while let Some(n) = rx.recv().await {
        received.push(n);
    }
    received.push(rx.recv().await.unwrap_or(0));
    received
}

/// A full channel refuses a `try_send`, and a closed one refuses every send.
async fn full() -> (bool, bool, usize, bool, bool) {
    let (tx, mut rx) = mpsc::channel::<u32>(1);
    let first = tx.try_send(1).is_ok();
    let second = tx.try_send(2).is_err();
    let queued = rx.len();
    rx.close();
    (
        first,
        second,
        queued,
        tx.try_send(3).is_err(),
        tx.send(4).await.is_err(),
    )
}

/// A value queued before the receiver closes can still be received.
fn closed_then_drained() -> (bool, u32, bool) {
    let (tx, mut rx) = mpsc::channel(2);
    tx.try_send(5).unwrap_or(());
    let open = !tx.is_closed() && !rx.is_closed();
    rx.close();
    let closed = open && tx.is_closed() && rx.is_closed();
    let value = match rx.try_recv() {
        Ok(value) => value,
        Err(_) => 0,
    };
    (closed, value, rx.try_recv().is_err() && rx.is_empty())
}

/// Dropping the receiver drops what is queued, and later sends fail.
async fn abandoned() -> (usize, bool) {
    let witness = Arc::new(());
    let (tx, rx) = mpsc::channel(2);
    if tx.send(Arc::clone(&witness)).await.is_err() {
        return (0, false);
    }
    drop(rx);
    let failed = tx.send(Arc::clone(&witness)).await.is_err();
    (Arc::strong_count(&witness), failed)
}

/// Each half moved into its own task, where the producer waits for room.
async fn relayed() -> Vec<u8> {
    let (tx, rx) = mpsc::channel(1);
    let producer = async move {
        let out = tx;
        for n in [1, 2, 3] {
            if out.send(n).await.is_err() {
                break;
            }
        }
    };
    let consumer = async move {
        let mut inbox = rx;
        let mut seen = Vec::new();
        while let Some(n) = inbox.recv().await {
            seen.push(n);
        }
        seen
    };
    let ((), seen) = tokio::join!(producer, consumer);
    seen
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!(
        "{:?} {:?} {:?} {:?} {:?}",
        drained().await,
        full().await,
        closed_then_drained(),
        abandoned().await,
        relayed().await
    );
}
