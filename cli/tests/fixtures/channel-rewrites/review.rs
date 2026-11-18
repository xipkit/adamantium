//! Tokio channels that `fix` leaves for review, each for its own reason.
use tokio::sync::{oneshot, watch};

fn keep<T>(value: T) -> T {
    value
}

/// A half passed to a function.
async fn passed() -> Option<u32> {
    let (passed_tx, passed_rx) = oneshot::channel();
    keep(passed_tx).send(1).unwrap_or(());
    passed_rx.await.ok()
}

/// A receive whose error propagates.
async fn propagated() -> Result<u32, oneshot::error::RecvError> {
    let (propagated_tx, propagated_rx) = oneshot::channel();
    propagated_tx.send(2).unwrap_or(());
    propagated_rx.await
}

/// A half used in a macro.
fn printed() {
    let (printed_tx, printed_rx) = oneshot::channel::<u32>();
    println!("{printed_tx:?}");
    drop(printed_rx);
}

/// Halves that no `let` binds.
fn unbound() -> bool {
    keep(oneshot::channel::<u32>()).0.is_closed()
}

/// A send whose failure is checked: Tokio's fails once no receiver is
/// alive, and `Watch::send` doesn't.
fn checked() -> bool {
    let (checked_tx, checked_rx) = watch::channel(0u32);
    drop(checked_rx);
    checked_tx.send(1).is_err()
}

/// A sender subscribed to after a send with no receiver alive, which Tokio
/// dropped: this prints 0, where a `Watch` would give 1.
fn subscribed() -> u32 {
    let (subscribed_tx, subscribed_rx) = watch::channel(0u32);
    drop(subscribed_rx);
    subscribed_tx.send(1).ok();
    let late = subscribed_tx.subscribe();
    *late.borrow()
}

/// A receiver that reads without marking and also waits.
async fn borrowed() -> (u32, bool) {
    let (borrowed_tx, mut borrowed_rx) = watch::channel(0u32);
    borrowed_tx.send(1).ok();
    let first = *borrowed_rx.borrow();
    drop(borrowed_tx);
    (first, borrowed_rx.changed().await.is_ok())
}

/// A send after the receiver is gone. Tokio drops the value, which ends
/// the wait on `done`; a `Watch` would keep it, and the wait would hang.
async fn unwatched() -> bool {
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let (unwatched_tx, unwatched_rx) = watch::channel(None);
    drop(unwatched_rx);
    unwatched_tx.send(Some(done_tx)).ok();
    let ended = done_rx.await.is_err();
    drop(unwatched_tx);
    ended
}

/// A read in place, which copies nothing, where `Watcher::get` would clone
/// the string.
fn compared() -> bool {
    let (compared_tx, compared_rx) = watch::channel(String::from("a"));
    let same = *compared_rx.borrow() == "a";
    drop(compared_tx);
    same
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    printed();
    println!(
        "{:?} {:?} {} {} {} {:?} {} {}",
        passed().await,
        propagated().await,
        unbound(),
        checked(),
        subscribed(),
        borrowed().await,
        unwatched().await,
        compared()
    );
}
