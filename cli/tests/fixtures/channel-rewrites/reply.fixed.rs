//! Tokio oneshot channels that `fix` rewrites to `Reply` pairs. The program
//! prints the same before and after.

/// A reply sent before the receiver waits for it.
async fn answered() -> Option<u32> {
    let (tx, rx) = ::adamantium::Reply::pair();
    tx.send(7).unwrap_or(());
    rx.receive().await
}

/// A reply dropped unsent ends the wait with nothing.
async fn abandoned() -> bool {
    let (tx, rx) = ::adamantium::Reply::<u32>::pair();
    drop(tx);
    rx.receive().await.is_none()
}

/// A send to a closed receiver hands the value back.
fn refused() -> (bool, bool, bool) {
    let (tx, rx) = ::adamantium::Reply::<u32>::pair();
    let open = tx.is_closed();
    rx.close();
    let closed = tx.is_closed();
    (open, closed, tx.send(1).is_err())
}

/// A value sent before the receiver closes can still be received.
fn sent_then_closed() -> u32 {
    let (tx, rx) = ::adamantium::Reply::pair();
    tx.send(3).unwrap_or(());
    rx.close();
    match rx.try_receive() {
        Ok(value) => value,
        Err(_) => 0,
    }
}

/// Nothing before the send, the value once, then nothing again.
fn polled() -> (bool, u32, bool) {
    let (tx, rx) = ::adamantium::Reply::pair();
    let early = rx.try_receive().is_err();
    tx.send(5).unwrap_or(());
    let value = rx.try_receive().unwrap_or(0);
    (early, value, rx.try_receive().is_err())
}

/// The sender learns that nobody waits any more.
async fn noticed() -> bool {
    let (tx, rx) = ::adamantium::Reply::<u32>::pair();
    drop(rx);
    tx.closed().await;
    tx.is_closed()
}

/// Each half moved into its own task.
async fn relayed() -> Result<u32, &'static str> {
    let (tx, rx) = ::adamantium::Reply::pair();
    let worker = async move {
        let reply = tx;
        reply.send(11).unwrap_or(());
    };
    let waiter = async move {
        let Some(value) = rx.receive().await else {
            return Err("no reply");
        };
        Ok(value)
    };
    worker.await;
    waiter.await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!(
        "{:?} {} {:?} {} {:?} {} {:?}",
        answered().await,
        abandoned().await,
        refused(),
        sent_then_closed(),
        polled(),
        noticed().await,
        relayed().await
    );
}
