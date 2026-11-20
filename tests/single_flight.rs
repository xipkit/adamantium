use adamantium::{Channel, SingleFlight};
use std::{
    future::{Future, pending, ready},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[derive(Clone, Default)]
struct Counts {
    started: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Counts {
    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    fn operation(
        &self,
        answers: &Channel<Result<u32, String>>,
    ) -> impl Future<Output = Result<u32, String>> + Send + 'static {
        self.started.fetch_add(1, Ordering::SeqCst);
        let dropped = Dropped(Arc::clone(&self.dropped));
        let answers = answers.clone();
        async move {
            let _dropped = dropped;
            answers.receive().await.unwrap_or(Err("closed".into()))
        }
    }
}

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn concurrent_calls_share_one_operation_and_its_output() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let mut first = pin!(flights.run("db", || counts.operation(&answers)));
    let mut second = pin!(flights.run("db", || counts.operation(&answers)));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    assert_eq!(counts.started(), 1, "the second call joins");
    assert_eq!(flights.len(), 1);

    answers.try_send(Ok(7)).unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(7)));
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(Ok(7)));
    assert_eq!(counts.started(), 1);
    assert!(flights.is_empty());
}

#[test]
fn a_call_that_joins_drops_its_operation_without_calling_it() {
    let flights = SingleFlight::new();
    let mut first = pin!(flights.run(1, pending::<u32>));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    let witness = Arc::new(());
    let held = Arc::clone(&witness);
    let mut called = false;
    {
        let mut second = pin!(flights.run(1, || {
            called = true;
            let _held = held;
            ready(0)
        }));
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
        assert_eq!(Arc::strong_count(&witness), 1, "the closure was dropped");
    }
    assert!(!called);
}

#[test]
fn calls_for_different_keys_run_separately() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let east = Channel::bounded(1);
    let west = Channel::bounded(1);
    let mut first = pin!(flights.run("east", || counts.operation(&east)));
    let mut second = pin!(flights.run("west", || counts.operation(&west)));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    assert_eq!((counts.started(), flights.len()), (2, 2));

    west.try_send(Ok(2)).unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(2)));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(flights.len(), 1);
}

#[test]
fn dropping_the_first_caller_leaves_the_operation_to_the_others() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let mut second = pin!(flights.run("db", || counts.operation(&answers)));
    {
        let mut first = pin!(flights.run("db", || counts.operation(&answers)));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    }
    assert_eq!(counts.dropped(), 0, "the second caller still waits");
    answers.try_send(Ok(3)).unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(3)));
    assert_eq!((counts.started(), counts.dropped()), (1, 1));
    assert!(flights.is_empty());
}

#[test]
fn a_wake_after_the_polling_caller_leaves_reaches_the_others() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut second = pin!(flights.run("db", || counts.operation(&answers)));
    {
        // The first caller polls the operation, so the operation's wake
        // is registered by its poll, not by the second caller's.
        let mut first = pin!(flights.run("db", || counts.operation(&answers)));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        let polled = second.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Pending);
    }
    answers.try_send(Ok(4)).unwrap();
    assert!(wakes.0.load(Ordering::SeqCst) > 0, "the second caller woke");
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(4)));
}

#[test]
fn dropping_the_last_caller_drops_the_operation_and_its_entry() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    {
        let mut first = pin!(flights.run("db", || counts.operation(&answers)));
        let mut second = pin!(flights.run("db", || counts.operation(&answers)));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    }
    assert_eq!((counts.started(), counts.dropped()), (1, 1));
    assert!(flights.is_empty());

    answers.try_send(Ok(5)).unwrap();
    let mut next = pin!(flights.run("db", || counts.operation(&answers)));
    assert_eq!(poll_once(next.as_mut()), Poll::Ready(Ok(5)));
    assert_eq!(counts.started(), 2);
}

#[test]
fn a_call_dropped_before_its_first_poll_never_joins() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let mut first = pin!(flights.run("db", || counts.operation(&answers)));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    drop(flights.run("db", || counts.operation(&answers)));
    answers.try_send(Ok(6)).unwrap();
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(Ok(6)));
    assert_eq!(counts.started(), 1);
}

#[test]
fn an_error_reaches_every_caller_and_the_next_call_tries_again() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let mut first = pin!(flights.run("db", || counts.operation(&answers)));
    let mut second = pin!(flights.run("db", || counts.operation(&answers)));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(poll_once(second.as_mut()), Poll::Pending);
    answers.try_send(Err("remote unavailable".into())).unwrap();
    let failed = Err("remote unavailable".to_string());
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(failed.clone()));
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(failed));
    assert!(flights.is_empty(), "an error is not kept");

    answers.try_send(Ok(8)).unwrap();
    let mut retry = pin!(flights.run("db", || counts.operation(&answers)));
    assert_eq!(poll_once(retry.as_mut()), Poll::Ready(Ok(8)));
    assert_eq!(counts.started(), 2);
}

#[test]
fn an_output_no_caller_takes_is_dropped_with_the_last_caller() {
    let flights = SingleFlight::new();
    let answers = Channel::<()>::bounded(1);
    let output = Arc::new(());
    let operation = || {
        let answers = answers.clone();
        let output = Arc::clone(&output);
        async move {
            answers.receive().await;
            output
        }
    };
    let mut first = pin!(flights.run(1, operation));
    let mut second = Box::pin(flights.run(1, operation));
    assert!(poll_once(first.as_mut()).is_pending());
    assert!(poll_once(second.as_mut()).is_pending());
    answers.try_send(()).unwrap();
    let Poll::Ready(mine) = poll_once(first.as_mut()) else {
        panic!("the operation finished");
    };
    assert_eq!(Arc::strong_count(&output), 3, "one kept for the second");
    drop(second);
    drop(mine);
    assert_eq!(Arc::strong_count(&output), 1);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn a_panic_reaches_the_polling_caller_and_every_other_caller() {
    let flights = SingleFlight::<u8, u32>::new();
    let answers = Channel::<()>::bounded(1);
    let operation = || {
        let answers = answers.clone();
        async move {
            answers.receive().await;
            panic!("push exploded")
        }
    };
    let mut first = pin!(flights.run(1, operation));
    let mut second = pin!(flights.run(1, operation));
    assert!(poll_once(first.as_mut()).is_pending());
    assert!(poll_once(second.as_mut()).is_pending());
    answers.try_send(()).unwrap();

    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"push exploded"));
    assert!(flights.is_empty());
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(second.as_mut()))).unwrap_err();
    let message = payload.downcast_ref::<&str>().unwrap();
    assert!(message.contains("panicked in another call"), "{message}");

    let mut next = pin!(flights.run(1, || ready(9)));
    assert_eq!(poll_once(next.as_mut()), Poll::Ready(9));
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn a_panic_creating_the_operation_leaves_no_entry() {
    let flights = SingleFlight::<u8, u32>::new();
    let mut first = pin!(flights.run(1, || -> std::future::Ready<u32> { panic!("no connection") }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"no connection"));
    assert!(flights.is_empty());
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn the_operation_cannot_call_for_its_own_key() {
    let flights = SingleFlight::<u8, u32>::new();
    let inner = flights.clone();
    let mut first = pin!(flights.run(1, || async move {
        let other = inner.run(2, || ready(2)).await;
        other + inner.run(1, || ready(1)).await
    }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    let message = payload.downcast_ref::<&str>().unwrap();
    assert!(
        message.contains("called by the operation for the same key"),
        "{message}"
    );
    assert!(flights.is_empty());
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn creating_the_operation_cannot_call_for_its_own_key() {
    let flights = SingleFlight::<u8, u32>::new();
    let inner = flights.clone();
    let mut first = pin!(flights.run(1, || {
        let mut nested = pin!(inner.run(1, || ready(1)));
        ready(u32::from(poll_once(nested.as_mut()).is_ready()))
    }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    let message = payload.downcast_ref::<&str>().unwrap();
    assert!(
        message.contains("called by the operation for the same key"),
        "{message}"
    );
    assert!(flights.is_empty());
}

struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        if std::mem::replace(&mut self.yielded, true) {
            Poll::Ready(12)
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[test]
fn an_operation_that_wakes_itself_is_not_polled_again_until_the_call_is() {
    let flights = SingleFlight::new();
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut call = pin!(flights.run(1, || YieldOnce { yielded: false }));
    let polled = call.as_mut().poll(&mut Context::from_waker(&waker));
    assert_eq!(polled, Poll::Pending, "the call returns to the executor");
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        1,
        "and asks to be polled again"
    );
    assert_eq!(poll_once(call.as_mut()), Poll::Ready(12));
}

#[test]
fn a_yielding_operation_lets_another_caller_take_its_turn() {
    let flights = SingleFlight::new();
    let mut first = pin!(flights.run(1, || YieldOnce { yielded: false }));
    let mut second = pin!(flights.run(1, || YieldOnce { yielded: false }));
    assert_eq!(poll_once(first.as_mut()), Poll::Pending);
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(12));
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(12));
}

type Call = Pin<Box<dyn Future<Output = u32> + Send>>;

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "the test hands a call to the operation"
)]
fn a_wake_the_polling_caller_leaves_behind_reaches_the_others() {
    let flights = SingleFlight::new();
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let second_slot: Arc<std::sync::Mutex<Option<Call>>> = Arc::default();
    let operation_waker: Arc<std::sync::Mutex<Option<Waker>>> = Arc::default();
    let (slot, saved, inner_waker) = (
        Arc::clone(&second_slot),
        Arc::clone(&operation_waker),
        waker.clone(),
    );
    let mut polls = 0;
    let operation = move || {
        std::future::poll_fn(move |cx| {
            polls += 1;
            match polls {
                1 => {
                    *saved.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                }
                2 => {
                    cx.waker().wake_by_ref();
                    let mut second = slot.lock().unwrap();
                    let second = second.as_mut().unwrap();
                    let polled = second.as_mut().poll(&mut Context::from_waker(&inner_waker));
                    assert!(polled.is_pending(), "the operation is being polled");
                    Poll::Pending
                }
                _ => Poll::Ready(5),
            }
        })
    };
    let mut first = Box::pin(flights.run(1, operation));
    assert!(poll_once(first.as_mut()).is_pending());
    let joined = flights.clone();
    let mut second: Call = Box::pin(async move { joined.run(1, || ready(0)).await });
    assert!(poll_once(second.as_mut()).is_pending());
    *second_slot.lock().unwrap() = Some(second);

    operation_waker.lock().unwrap().take().unwrap().wake();
    assert!(poll_once(first.as_mut()).is_pending());
    let looked = wakes.0.load(Ordering::SeqCst);
    drop(first);
    assert!(
        wakes.0.load(Ordering::SeqCst) > looked,
        "the second caller was woken to take the operation"
    );
    let mut second = second_slot.lock().unwrap().take().unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(5));
    assert!(flights.is_empty());
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has one thread")]
#[allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "a second caller joins from another thread during the first poll"
)]
fn a_wake_during_the_first_poll_reaches_the_others_when_the_creator_leaves() {
    let flights = SingleFlight::new();
    let wakes = Arc::new(Wakes::default());
    let second_slot: Arc<std::sync::Mutex<Option<Call>>> = Arc::default();
    let (slot, joined, second_wakes) = (
        Arc::clone(&second_slot),
        flights.clone(),
        Arc::clone(&wakes),
    );
    let mut polls = 0;
    let operation = move || {
        std::future::poll_fn(move |cx| {
            polls += 1;
            if polls > 1 {
                return Poll::Ready(5);
            }
            cx.waker().wake_by_ref();
            let (joined, second_wakes) = (joined.clone(), Arc::clone(&second_wakes));
            let second = std::thread::spawn(move || {
                let mut second: Call = Box::pin(async move { joined.run(1, || ready(0)).await });
                let waker = Waker::from(second_wakes);
                let polled = second.as_mut().poll(&mut Context::from_waker(&waker));
                assert!(polled.is_pending(), "the operation is being polled");
                second
            })
            .join()
            .unwrap();
            *slot.lock().unwrap() = Some(second);
            Poll::Pending
        })
    };
    let mut first = Box::pin(flights.run(1, operation));
    assert!(poll_once(first.as_mut()).is_pending());
    let looked = wakes.0.load(Ordering::SeqCst);
    drop(first);
    assert!(
        wakes.0.load(Ordering::SeqCst) > looked,
        "the second caller was woken to take the operation"
    );
    let mut second = second_slot.lock().unwrap().take().unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(5));
    assert!(flights.is_empty());
}

#[test]
fn a_call_polled_between_the_polls_of_the_operation_is_not_nested() {
    let flights = SingleFlight::new();
    let answers = Channel::<u32>::bounded(1);
    let operation = || {
        let answers = answers.clone();
        async move { answers.receive().await.unwrap_or(0) }
    };
    let mut first = pin!(flights.run(1, operation));
    assert!(poll_once(first.as_mut()).is_pending());
    let mut second = pin!(flights.run(1, operation));
    assert!(poll_once(second.as_mut()).is_pending());
    answers.try_send(10).unwrap();
    assert_eq!(poll_once(second.as_mut()), Poll::Ready(10));
    assert_eq!(poll_once(first.as_mut()), Poll::Ready(10));
}

#[test]
fn a_call_with_a_send_operation_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let flights = SingleFlight::<String, u32>::new();
    let call = flights.run("key".into(), || ready(1));
    assert_send(&call);
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    assert_send_sync(&flights);
}

#[test]
fn many_callers_share_one_operation_whatever_order_they_poll_in() {
    let flights = SingleFlight::new();
    let counts = Counts::default();
    let answers = Channel::bounded(1);
    let mut calls = (0..5)
        .map(|_| Box::pin(flights.run("db", || counts.operation(&answers))))
        .collect::<Vec<_>>();
    for call in &mut calls {
        assert!(poll_once(call.as_mut()).is_pending());
    }
    answers.try_send(Ok(11)).unwrap();
    for call in calls.iter_mut().rev() {
        assert_eq!(poll_once(call.as_mut()), Poll::Ready(Ok(11)));
    }
    assert_eq!(counts.started(), 1);
}

#[cfg(feature = "tokio")]
mod until {
    use super::*;
    use adamantium::{Cancellation, ScopeError, time};
    use std::time::Duration;

    fn cancelled() -> Cancellation {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation
    }

    #[tokio::test]
    async fn a_request_already_present_starts_no_operation() {
        let flights = SingleFlight::<&str, Result<u32, String>>::new();
        let mut called = false;
        let result = flights
            .run_until("db", &cancelled(), None, || {
                called = true;
                ready(Ok(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
        assert!(!called);
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn a_request_already_present_joins_no_operation() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        {
            let mut first = pin!(flights.run("db", || counts.operation(&answers)));
            assert_eq!(poll_once(first.as_mut()), Poll::Pending);
            let result = flights
                .run_until("db", &cancelled(), None, || counts.operation(&answers))
                .await;
            assert_eq!(result, Err(ScopeError::Cancelled));
            assert_eq!(counts.started(), 1);
        }
        // The operation would survive if the cancelled call had joined.
        assert_eq!(counts.dropped(), 1);
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn an_expired_deadline_starts_no_operation() {
        let flights = SingleFlight::<&str, Result<u32, String>>::new();
        let result = flights
            .run_until("db", &Cancellation::new(), Some(time::now()), || {
                ready(Ok(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn cancellation_wins_over_an_equally_ready_deadline() {
        let flights = SingleFlight::<&str, Result<u32, String>>::new();
        let result = flights
            .run_until("db", &cancelled(), Some(time::now()), || ready(Ok(1)))
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
    }

    #[tokio::test]
    async fn a_request_wins_over_an_output_not_yet_taken() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let cancellation = Cancellation::new();
        let mut first = pin!(flights.run("db", || counts.operation(&answers)));
        let mut second =
            pin!(flights.run_until("db", &cancellation, None, || { counts.operation(&answers) }));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
        answers.try_send(Ok(2)).unwrap();
        assert_eq!(poll_once(first.as_mut()), Poll::Ready(Ok(2)));
        cancellation.cancel();
        assert_eq!(
            poll_once(second.as_mut()),
            Poll::Ready(Err(ScopeError::Cancelled))
        );
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn the_error_of_the_operation_reaches_every_caller_as_failed() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let (first_stop, second_stop) = (Cancellation::new(), Cancellation::new());
        let mut first =
            pin!(flights.run_until("db", &first_stop, None, || { counts.operation(&answers) }));
        let mut second =
            pin!(flights.run_until("db", &second_stop, None, || { counts.operation(&answers) }));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
        answers.try_send(Err("down".into())).unwrap();
        let failed = Poll::Ready(Err(ScopeError::Failed("down".to_string())));
        assert_eq!(poll_once(second.as_mut()), failed);
        assert_eq!(poll_once(first.as_mut()), failed);
        assert_eq!(counts.started(), 1);
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_first_caller_leaves_the_operation_to_the_others() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let cancellation = Cancellation::new();
        let mut second = pin!(flights.run("db", || counts.operation(&answers)));
        let mut first =
            pin!(flights.run_until("db", &cancellation, None, || { counts.operation(&answers) }));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
        cancellation.cancel();
        assert_eq!(
            poll_once(first.as_mut()),
            Poll::Ready(Err(ScopeError::Cancelled))
        );
        assert_eq!((counts.started(), counts.dropped()), (1, 0));
        assert_eq!(flights.len(), 1);

        answers.try_send(Ok(3)).unwrap();
        assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(3)));
        assert_eq!((counts.started(), counts.dropped()), (1, 1));
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn a_joining_caller_past_its_deadline_leaves_the_operation_to_the_first() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let deadline = time::now() + Duration::from_millis(20);
        let cancellation = Cancellation::new();
        let mut first = pin!(flights.run("db", || counts.operation(&answers)));
        let mut second = pin!(flights.run_until("db", &cancellation, Some(deadline), || {
            counts.operation(&answers)
        }));
        assert_eq!(poll_once(first.as_mut()), Poll::Pending);
        assert_eq!(poll_once(second.as_mut()), Poll::Pending);
        let result = second.as_mut().await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert_eq!((counts.started(), counts.dropped()), (1, 0));

        answers.try_send(Ok(4)).unwrap();
        assert_eq!(poll_once(first.as_mut()), Poll::Ready(Ok(4)));
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_caller_woken_to_poll_the_operation_hands_it_on() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let cancellation = Cancellation::new();
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut second = pin!(flights.run("db", || counts.operation(&answers)));
        {
            let mut first = pin!(
                flights.run_until("db", &cancellation, None, || { counts.operation(&answers) })
            );
            assert_eq!(poll_once(first.as_mut()), Poll::Pending);
            let polled = second.as_mut().poll(&mut Context::from_waker(&waker));
            assert_eq!(polled, Poll::Pending);
            // The operation is woken, then its caller is cancelled before
            // it polls it again.
            let looked = wakes.0.load(Ordering::SeqCst);
            answers.try_send(Ok(5)).unwrap();
            cancellation.cancel();
            assert_eq!(
                poll_once(first.as_mut()),
                Poll::Ready(Err(ScopeError::Cancelled))
            );
            assert!(
                wakes.0.load(Ordering::SeqCst) > looked,
                "the second caller was woken to take the operation"
            );
            assert_eq!(answers.len(), 1, "the first caller left it unpolled");
        }
        assert_eq!(poll_once(second.as_mut()), Poll::Ready(Ok(5)));
        assert_eq!((counts.started(), counts.dropped()), (1, 1));
        assert!(flights.is_empty());
    }

    #[tokio::test]
    async fn the_last_caller_to_stop_drops_the_operation_and_its_entry() {
        let flights = SingleFlight::new();
        let counts = Counts::default();
        let answers = Channel::bounded(1);
        let (first_stop, second_stop) = (Cancellation::new(), Cancellation::new());
        let deadline = time::now() + Duration::from_millis(20);
        {
            let mut first =
                pin!(flights.run_until("db", &first_stop, None, || { counts.operation(&answers) }));
            let mut second = pin!(flights.run_until("db", &second_stop, Some(deadline), || {
                counts.operation(&answers)
            }));
            assert_eq!(poll_once(first.as_mut()), Poll::Pending);
            assert_eq!(poll_once(second.as_mut()), Poll::Pending);
            first_stop.cancel();
            assert_eq!(
                poll_once(first.as_mut()),
                Poll::Ready(Err(ScopeError::Cancelled))
            );
            assert_eq!(counts.dropped(), 0, "the second caller still waits");
            assert_eq!(second.await, Err(ScopeError::DeadlineExceeded));
            assert_eq!((counts.started(), counts.dropped()), (1, 1));
            assert!(flights.is_empty());
        }

        answers.try_send(Ok(6)).unwrap();
        let result = flights
            .run_until("db", &Cancellation::new(), None, || {
                counts.operation(&answers)
            })
            .await;
        assert_eq!(result, Ok(6));
        assert_eq!(counts.started(), 2);
    }

    #[test]
    fn a_call_with_a_send_operation_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let flights = SingleFlight::<String, Result<u32, String>>::new();
        let cancellation = Cancellation::new();
        let call = flights.run_until("key".into(), &cancellation, None, || ready(Ok(1)));
        assert_send(&call);
    }
}
