//! Tokio watch channels that `fix` rewrites to a `Watch` and its watchers.
//! The program prints the same before and after.

/// The receiver has seen the initial value, so a wait after the sender is
/// gone ends at once.
async fn initial_is_seen() -> bool {
    let tx = ::adamantium::Watch::new(1u32);
    let mut rx = tx.subscribe();
    drop(tx);
    rx.receive().await.is_none()
}

/// The last value sent before the sender went is still received, by the
/// receiver and by a clone that keeps its own place.
async fn last_value() -> Vec<u32> {
    let tx = ::adamantium::Watch::new(1u32);
    let mut rx = tx.subscribe();
    let mut copy = rx.clone();
    tx.send(2).ok();
    tx.send(3).ok();
    drop(tx);
    let mut seen = Vec::new();
    while rx.receive().await.is_some() {
        seen.push(rx.get());
    }
    if copy.receive().await.is_some() {
        seen.push(copy.get());
    }
    seen
}

/// The channel closes when the last sender clone goes.
async fn senders() -> (bool, u32, bool) {
    let tx = ::adamantium::Watch::new(0u32);
    let mut rx = tx.subscribe();
    let other = tx.clone();
    drop(tx);
    other.send(4).ok();
    let changed = rx.receive().await.is_some();
    let value = rx.get();
    drop(other);
    (changed, value, rx.receive().await.is_none())
}

/// A receiver that only reads the latest value.
fn latest() -> u32 {
    let tx = ::adamantium::Watch::new(5u32);
    let mut rx = tx.subscribe();
    tx.send(6).unwrap_or(());
    rx.get()
}

/// A receiver moved into a task.
async fn worker() -> u32 {
    let tx = ::adamantium::Watch::new(0u32);
    let rx = tx.subscribe();
    let task = async move {
        let mut rx = rx;
        let mut total = 0;
        while rx.receive().await.is_some() {
            total += rx.get();
        }
        total
    };
    tx.send(8).ok();
    drop(tx);
    task.await
}

/// A closed channel ends the function with an error.
async fn propagated() -> Result<u32, &'static str> {
    let tx = ::adamantium::Watch::new(0u32);
    let mut rx = tx.subscribe();
    tx.send(9).ok();
    rx.receive().await.ok_or_else(|| "closed")?;
    let value = rx.get();
    drop(tx);
    rx.receive().await.ok_or_else(|| "closed")?;
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
