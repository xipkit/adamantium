//! Tokio watch channels that `fix` rewrites to a `Watch` and its watchers.
//! The program prints the same before and after.
use tokio::sync::watch;

/// The receiver has seen the initial value, so a wait after the sender is
/// gone ends at once.
async fn initial_is_seen() -> bool {
    let (tx, mut rx) = watch::channel(1u32);
    drop(tx);
    rx.changed().await.is_err()
}

/// The last value sent before the sender went is still received, by the
/// receiver and by a clone that keeps its own place.
async fn last_value() -> Vec<u32> {
    let (tx, mut rx) = watch::channel(1u32);
    let mut copy = rx.clone();
    tx.send(2).ok();
    tx.send(3).ok();
    drop(tx);
    let mut seen = Vec::new();
    while rx.changed().await.is_ok() {
        seen.push(*rx.borrow_and_update());
    }
    if copy.changed().await.is_ok() {
        seen.push(copy.borrow_and_update().clone());
    }
    seen
}

/// The channel closes when the last sender clone goes.
async fn senders() -> (bool, u32, bool) {
    let (tx, mut rx) = watch::channel(0u32);
    let other = tx.clone();
    drop(tx);
    other.send(4).ok();
    let changed = rx.changed().await.is_ok();
    let value = *rx.borrow_and_update();
    drop(other);
    (changed, value, rx.changed().await.is_err())
}

/// A receiver that only reads the latest value.
fn latest() -> u32 {
    let (tx, rx) = watch::channel(5u32);
    tx.send(6).unwrap_or(());
    *rx.borrow()
}

/// A receiver moved into a task.
async fn worker() -> u32 {
    let (tx, rx) = watch::channel(0u32);
    let task = async move {
        let mut rx = rx;
        let mut total = 0;
        while rx.changed().await.is_ok() {
            total += *rx.borrow_and_update();
        }
        total
    };
    tx.send(8).ok();
    drop(tx);
    task.await
}

/// A closed channel ends the function with an error.
async fn propagated() -> Result<u32, &'static str> {
    let (tx, mut rx) = watch::channel(0u32);
    tx.send(9).ok();
    rx.changed().await.map_err(|_| "closed")?;
    let value = *rx.borrow_and_update();
    drop(tx);
    rx.changed().await.map_err(|_| "closed")?;
    Ok(value)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!(
        "{} {:?} {:?} {} {} {:?}",
        initial_is_seen().await,
        last_value().await,
        senders().await,
        latest(),
        worker().await,
        propagated().await
    );
}
