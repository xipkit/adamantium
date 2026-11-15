//! These tests run with and without the `tokio` feature, so the one-shot
//! reply stays available to crates that leave Tokio out.

#[cfg(feature = "tokio")]
use adamantium::{Cancellation, Channel, ReceiveError, Tasks, time};
use adamantium::{Closed, Reply, ReplyReceiver, TryReceiveError};
#[cfg(feature = "tokio")]
use std::time::Duration;
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl WakeCounter {
    fn wakes(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>, counter: &Arc<WakeCounter>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::clone(counter));
    future.poll(&mut Context::from_waker(&waker))
}

/// Polls a new future once.
fn now<F: Future>(future: F) -> Poll<F::Output> {
    poll_once(pin!(future), &Arc::new(WakeCounter::default()))
}

#[test]
fn a_sent_reply_is_received_once() {
    let (reply, answer) = Reply::pair();
    assert!(!answer.is_ready());
    assert_eq!(answer.try_receive(), Err(TryReceiveError::Empty));
    assert_eq!(reply.send("pong"), Ok(()));
    assert!(answer.is_ready());
    assert!(answer.is_closed(), "no second reply can come");
    assert_eq!(now(answer.receive()), Poll::Ready(Some("pong")));
    assert!(!answer.is_ready());
    assert_eq!(now(answer.receive()), Poll::Ready(None));
    assert_eq!(answer.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn try_receive_takes_a_sent_reply() {
    let (reply, answer) = Reply::pair();
    reply.send(3).unwrap();
    assert_eq!(answer.try_receive(), Ok(3));
    assert_eq!(answer.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn a_reply_dropped_without_sending_ends_the_wait() {
    let (reply, answer) = Reply::<u8>::pair();
    let counter = Arc::new(WakeCounter::default());
    let mut waiting = pin!(answer.receive());
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
    drop(reply);
    assert!(counter.wakes() > 0);
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Ready(None));
    assert_eq!(answer.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn a_waiting_receive_wakes_when_the_reply_is_sent() {
    let (reply, answer) = Reply::pair();
    let counter = Arc::new(WakeCounter::default());
    let mut waiting = pin!(answer.receive());
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
    assert_eq!(counter.wakes(), 0);
    reply.send(9).unwrap();
    assert!(counter.wakes() > 0);
    assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Ready(Some(9)));
}

#[test]
fn dropping_a_waiting_receive_keeps_the_reply() {
    let (reply, answer) = Reply::pair();
    let counter = Arc::new(WakeCounter::default());
    {
        let mut waiting = pin!(answer.receive());
        assert_eq!(poll_once(waiting.as_mut(), &counter), Poll::Pending);
    }
    reply.send(4).unwrap();
    assert_eq!(now(answer.receive()), Poll::Ready(Some(4)));
}

#[test]
fn a_dropped_receiver_hands_the_value_back() {
    let (reply, answer) = Reply::pair();
    assert!(!reply.is_closed());
    let counter = Arc::new(WakeCounter::default());
    {
        let mut closed = pin!(reply.closed());
        assert_eq!(poll_once(closed.as_mut(), &counter), Poll::Pending);
        drop(answer);
        assert!(counter.wakes() > 0);
        assert_eq!(poll_once(closed.as_mut(), &counter), Poll::Ready(()));
    }
    assert!(reply.is_closed());
    assert_eq!(
        reply.send(String::from("late")),
        Err(Closed(String::from("late")))
    );
}

#[test]
fn closing_the_receiver_refuses_the_reply() {
    let (reply, answer) = Reply::pair();
    assert!(answer.close());
    assert!(!answer.close());
    assert!(reply.is_closed());
    assert_eq!(now(reply.closed()), Poll::Ready(()));
    assert_eq!(reply.send(1), Err(Closed(1)));
    assert_eq!(answer.try_receive(), Err(TryReceiveError::Closed));
}

#[test]
fn closing_after_the_reply_keeps_it() {
    let (reply, answer) = Reply::pair();
    reply.send(5).unwrap();
    assert!(!answer.close(), "the sent reply already closed it");
    assert_eq!(answer.try_receive(), Ok(5));
}

#[test]
fn halves_cross_tasks_and_describe_themselves() {
    fn crosses_tasks<T: Send + Sync + 'static>() {}
    crosses_tasks::<Reply<String>>();
    crosses_tasks::<ReplyReceiver<String>>();

    let (reply, answer) = Reply::<u8>::pair();
    assert_eq!(format!("{reply:?}"), "Reply { closed: false }");
    assert_eq!(
        format!("{answer:?}"),
        "ReplyReceiver { ready: false, closed: false }"
    );
    reply.send(1).unwrap();
    assert_eq!(
        format!("{answer:?}"),
        "ReplyReceiver { ready: true, closed: true }"
    );
}

#[cfg(feature = "tokio")]
fn past() -> Option<Instant> {
    Some(time::now() - Duration::from_millis(1))
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread")]
async fn requests_carry_their_replies_across_tasks() {
    struct Double {
        n: u64,
        reply: Reply<u64>,
    }
    let requests = Channel::<Double>::bounded(4);
    let answers = Tasks::<&str>::run(async |t| {
        let server = requests.clone();
        t.spawn(async move {
            while let Some(Double { n, reply }) = server.receive().await {
                reply.send(n * 2).map_err(|_| "asker left")?;
            }
            Ok(())
        });
        let mut askers = Vec::new();
        for n in 0..20 {
            let requests = requests.clone();
            askers.push(t.spawn(async move {
                let (reply, answer) = Reply::pair();
                requests
                    .send(Double { n, reply })
                    .await
                    .map_err(|_| "closed")?;
                answer.receive().await.ok_or("no reply")
            }));
        }
        let mut answers = Vec::new();
        for asker in askers {
            answers.push(t.value(asker).await?);
        }
        requests.close();
        Ok(answers)
    })
    .await;
    assert_eq!(answers, Ok((0..20).map(|n| n * 2).collect()));
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_at_the_deadline_and_keeps_a_later_reply() {
    let (reply, answer) = Reply::pair();
    let cancellation = Cancellation::new();
    let start = time::now();
    assert_eq!(
        answer
            .receive_until(&cancellation, Some(start + Duration::from_secs(5)))
            .await,
        Err(ReceiveError::DeadlineExceeded)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    reply.send(8).unwrap();
    assert_eq!(answer.receive_until(&cancellation, None).await, Ok(8));
    assert_eq!(
        answer.receive_until(&cancellation, None).await,
        Err(ReceiveError::Closed)
    );
}

#[cfg(feature = "tokio")]
#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn receive_until_ends_when_cancelled() {
    let (_reply, answer) = Reply::<u8>::pair();
    let cancellation = Cancellation::new();
    let start = time::now();
    let canceller = cancellation.clone();
    let (received, ()) = tokio::join!(
        answer.receive_until(&cancellation, Some(start + Duration::from_secs(3600))),
        async move {
            time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        },
    );
    assert_eq!(received, Err(ReceiveError::Cancelled));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn receive_until_prefers_shutdown_to_a_sent_reply() {
    let (reply, answer) = Reply::pair();
    reply.send(1).unwrap();
    let cancellation = Cancellation::new();
    // Repeated, so an unordered race between the two would show.
    for _ in 0..16 {
        assert_eq!(
            answer.receive_until(&cancellation, past()).await,
            Err(ReceiveError::DeadlineExceeded)
        );
    }
    cancellation.cancel();
    for _ in 0..16 {
        assert_eq!(
            answer.receive_until(&cancellation, past()).await,
            Err(ReceiveError::Cancelled)
        );
    }
    assert_eq!(
        answer.try_receive(),
        Ok(1),
        "shutdown left the reply in place"
    );
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn receive_until_reports_a_dropped_reply() {
    let (reply, answer) = Reply::<u8>::pair();
    drop(reply);
    assert_eq!(
        answer.receive_until(&Cancellation::new(), None).await,
        Err(ReceiveError::Closed)
    );
}
