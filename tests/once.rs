use adamantium::{Channel, Once};
use std::{
    future::{Future, ready},
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

#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// A call polled only through one waker that counts its wakes. A primitive
// wakes the waker of the latest poll, so a second waker would hide them.
struct Watched<'a, T> {
    call: Pin<Box<dyn Future<Output = T> + 'a>>,
    wakes: Arc<Wakes>,
    waker: Waker,
}

impl<'a, T> Watched<'a, T> {
    fn new(call: impl Future<Output = T> + 'a) -> Self {
        let wakes = Arc::new(Wakes::default());
        Self {
            call: Box::pin(call),
            waker: Waker::from(Arc::clone(&wakes)),
            wakes,
        }
    }

    fn poll(&mut self) -> Poll<T> {
        self.call
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    fn woken(&self) -> usize {
        self.wakes.0.load(Ordering::SeqCst)
    }
}

// Counts initializers. Each one waits for an answer on the channel.
#[derive(Clone, Default)]
struct Inits {
    started: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Inits {
    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    fn start(
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

#[test]
fn concurrent_first_calls_run_one_initializer_and_share_its_value() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());
    assert_eq!(inits.started(), 1);

    answers.try_send(Ok(7)).unwrap();
    assert_eq!(first.woken(), 1);
    let Poll::Ready(Ok(made)) = first.poll() else {
        panic!("the initializer has its answer");
    };
    assert_eq!(second.woken(), 1);
    let Poll::Ready(Ok(shared)) = second.poll() else {
        panic!("the value is set");
    };
    assert!(std::ptr::eq(made, shared));
    assert_eq!((*made, inits.started()), (7, 1));
}

#[test]
fn a_filled_cell_drops_the_initializer_without_calling_it() {
    let cell = Once::new();
    assert_eq!(
        poll_once(pin!(cell.get_or_init(|| ready(1)))),
        Poll::Ready(&1)
    );
    let mut called = false;
    let call = cell.get_or_init(|| {
        called = true;
        ready(2)
    });
    assert_eq!(poll_once(pin!(call)), Poll::Ready(&1));
    assert!(!called);
}

#[test]
fn get_reads_the_value_without_waiting() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    assert_eq!(cell.get(), None);
    let mut call = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(call.poll().is_pending());
    assert_eq!(cell.get(), None);
    answers.try_send(Ok(3)).unwrap();
    assert_eq!(call.poll(), Poll::Ready(Ok(&3)));
    assert_eq!(cell.get(), Some(&3));
}

#[test]
fn a_failed_initializer_leaves_the_cell_empty_and_one_waiting_call_takes_over() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut third = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());
    assert!(third.poll().is_pending());

    answers.try_send(Err("down".into())).unwrap();
    assert_eq!(first.poll(), Poll::Ready(Err("down".into())));
    assert_eq!(cell.get(), None);
    assert_eq!(second.woken() + third.woken(), 1);
    let (mut taking, mut waiting) = if second.woken() == 1 {
        (second, third)
    } else {
        (third, second)
    };
    assert!(taking.poll().is_pending());
    assert!(waiting.poll().is_pending());
    assert_eq!(inits.started(), 2);

    answers.try_send(Ok(5)).unwrap();
    assert_eq!(taking.poll(), Poll::Ready(Ok(&5)));
    assert_eq!(waiting.woken(), 1);
    assert_eq!(waiting.poll(), Poll::Ready(Ok(&5)));
    assert_eq!(inits.started(), 2);
}

#[test]
fn a_dropped_initializing_call_hands_over_to_a_waiting_call() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());

    drop(first);
    assert_eq!(inits.dropped(), 1);
    assert_eq!(second.woken(), 1);
    assert!(second.poll().is_pending());
    assert_eq!(inits.started(), 2);
    answers.try_send(Ok(4)).unwrap();
    assert_eq!(second.poll(), Poll::Ready(Ok(&4)));
}

#[test]
fn a_waiting_call_that_leaves_after_its_wake_passes_it_on() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut third = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());
    assert!(third.poll().is_pending());

    answers.try_send(Err("down".into())).unwrap();
    assert!(first.poll().is_ready());
    let (woken, mut other) = if second.woken() == 1 {
        (second, third)
    } else {
        (third, second)
    };
    assert_eq!(other.woken(), 0);
    drop(woken);
    assert_eq!(other.woken(), 1);
    assert!(other.poll().is_pending());
    assert_eq!(inits.started(), 2);
    answers.try_send(Ok(6)).unwrap();
    assert_eq!(other.poll(), Poll::Ready(Ok(&6)));
}

#[test]
fn a_waiting_call_that_leaves_does_not_disturb_the_initializer() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());
    drop(second);
    assert_eq!(first.woken(), 0);
    answers.try_send(Ok(8)).unwrap();
    assert_eq!(first.poll(), Poll::Ready(Ok(&8)));
    assert_eq!((inits.started(), inits.dropped()), (1, 1));
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn a_panicking_initializer_leaves_the_cell_to_a_waiting_call() {
    let cell = Once::new();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_init(|| async {
        answers.receive().await;
        std::panic::panic_any("initializer failed")
    }));
    let mut second = Watched::new(cell.get_or_init(|| ready(9)));
    assert!(first.poll().is_pending());
    assert!(second.poll().is_pending());

    answers.try_send(()).unwrap();
    let payload = catch_unwind(AssertUnwindSafe(|| first.poll())).unwrap_err();
    assert_eq!(payload.downcast_ref(), Some(&"initializer failed"));
    assert_eq!(cell.get(), None);
    assert_eq!(second.woken(), 1);
    assert_eq!(second.poll(), Poll::Ready(&9));
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn a_panic_creating_the_initializer_leaves_the_cell_empty() {
    let cell = Once::<u32>::new();
    let payload = catch_unwind(AssertUnwindSafe(|| {
        poll_once(pin!(cell.get_or_init(|| -> std::future::Ready<u32> {
            std::panic::panic_any("no initializer")
        })))
    }))
    .unwrap_err();
    assert_eq!(payload.downcast_ref(), Some(&"no initializer"));
    assert_eq!(
        poll_once(pin!(cell.get_or_init(|| ready(1)))),
        Poll::Ready(&1)
    );
}

const SELF_WAIT: &str = "adamantium: Once::get_or_init was called by the initializer of the same cell; \
it would wait for its own value, so call it from outside the initializer";

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn the_initializer_cannot_call_for_its_own_cell() {
    let cell = Once::new();
    let mut first = pin!(cell.get_or_init(|| async { *cell.get_or_init(|| ready(1)).await + 1 }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    assert_eq!(payload.downcast_ref(), Some(&SELF_WAIT));
    // The refused call gave up its claim, so the cell can still be filled.
    assert_eq!(
        poll_once(pin!(cell.get_or_init(|| ready(3)))),
        Poll::Ready(&3)
    );
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn creating_the_initializer_cannot_call_for_its_own_cell() {
    let cell = Once::new();
    let mut first = pin!(cell.get_or_init(|| {
        let nested = poll_once(pin!(cell.get_or_init(|| ready(1))));
        ready(u32::from(nested.is_ready()))
    }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(first.as_mut()))).unwrap_err();
    assert_eq!(payload.downcast_ref(), Some(&SELF_WAIT));
    assert_eq!(cell.get(), None);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
fn an_initializer_can_initialize_another_cell_but_not_a_cycle() {
    let outer = Once::new();
    let inner = Once::new();
    let value =
        poll_once(pin!(outer.get_or_init(|| async {
            *inner.get_or_init(|| ready(2)).await + 1
        })));
    assert_eq!(value, Poll::Ready(&3));

    let (first, second) = (Once::<u32>::new(), Once::<u32>::new());
    let mut cycle = pin!(first.get_or_init(|| async {
        *second
            .get_or_init(|| async { *first.get_or_init(|| ready(1)).await })
            .await
    }));
    let payload = catch_unwind(AssertUnwindSafe(|| poll_once(cycle.as_mut()))).unwrap_err();
    assert_eq!(payload.downcast_ref(), Some(&SELF_WAIT));
    assert_eq!((first.get(), second.get()), (None, None));
}

#[test]
fn a_call_polled_between_the_polls_of_the_initializer_waits() {
    let cell = Once::new();
    let inits = Inits::default();
    let answers = Channel::bounded(1);
    let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(first.poll().is_pending());
    let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
    assert!(second.poll().is_pending());
    answers.try_send(Ok(2)).unwrap();
    assert_eq!(first.poll(), Poll::Ready(Ok(&2)));
    assert_eq!(second.poll(), Poll::Ready(Ok(&2)));
}

#[test]
fn an_initializer_can_borrow_from_its_caller() {
    let cell = Once::new();
    let name = String::from("primary");
    let value = poll_once(pin!(cell.get_or_init(|| async { name.as_str() })));
    assert_eq!(value, Poll::Ready(&"primary"));
}

#[test]
fn clones_share_the_cell() {
    let cell = Once::new();
    let clone = cell.clone();
    assert_eq!(
        poll_once(pin!(clone.get_or_init(|| ready(5)))),
        Poll::Ready(&5)
    );
    assert_eq!(cell.get(), Some(&5));
    assert_eq!(Once::<u32>::default().get(), None);
}

#[test]
fn debug_shows_the_value() {
    let cell = Once::new();
    assert_eq!(format!("{cell:?}"), "Once { value: None }");
    assert_eq!(
        poll_once(pin!(cell.get_or_init(|| ready(5)))),
        Poll::Ready(&5)
    );
    assert_eq!(format!("{cell:?}"), "Once { value: Some(5) }");
}

#[test]
fn a_call_with_a_send_initializer_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    let cell = Once::<String>::new();
    assert_send_sync(&cell);
    let call = cell.get_or_init(|| ready(String::new()));
    assert_send(&call);
    let call = cell.get_or_try_init(|| ready(Ok::<_, String>(String::new())));
    assert_send(&call);
}

#[cfg(feature = "tokio")]
mod until {
    use super::*;
    use adamantium::{Cancellation, ScopeError, time};

    fn cancelled() -> Cancellation {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation
    }

    #[test]
    fn a_task_that_calls_until_is_send() {
        fn spawnable<F: Future + Send + 'static>(_: F) {}
        let cell = Once::<String>::new();
        spawnable(async move {
            let cancellation = Cancellation::new();
            cell.get_or_try_init_until(&cancellation, None, || {
                ready(Ok::<_, String>(String::new()))
            })
            .await
            .map(String::len)
        });
    }

    #[tokio::test]
    async fn a_request_already_present_calls_no_initializer() {
        let cell = Once::<u32>::new();
        let mut called = false;
        let result = cell
            .get_or_try_init_until(&cancelled(), None, || {
                called = true;
                ready(Ok::<_, String>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
        assert!(!called);
        // The cancelled call took no claim.
        assert_eq!(cell.get_or_init(|| ready(2)).await, &2);
    }

    #[tokio::test]
    async fn a_request_already_present_wins_over_a_filled_cell() {
        let cell = Once::new();
        cell.get_or_init(|| ready(1)).await;
        let result = cell
            .get_or_try_init_until(&cancelled(), None, || ready(Ok::<_, String>(2)))
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
    }

    #[tokio::test]
    async fn an_expired_deadline_calls_no_initializer() {
        let cell = Once::<u32>::new();
        let result = cell
            .get_or_try_init_until(&Cancellation::new(), Some(time::now()), || {
                ready(Ok::<_, String>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert_eq!(cell.get(), None);
    }

    #[tokio::test]
    async fn cancellation_wins_over_an_equally_ready_deadline() {
        let cell = Once::<u32>::new();
        let result = cell
            .get_or_try_init_until(&cancelled(), Some(time::now()), || {
                ready(Ok::<_, String>(1))
            })
            .await;
        assert_eq!(result, Err(ScopeError::Cancelled));
    }

    #[tokio::test]
    async fn the_error_of_the_initializer_is_failed() {
        let cell = Once::<u32>::new();
        let result = cell
            .get_or_try_init_until(&Cancellation::new(), None, || ready(Err("down".to_owned())))
            .await;
        assert_eq!(result, Err(ScopeError::Failed("down".to_owned())));
        assert_eq!(cell.get(), None);
    }

    #[tokio::test]
    async fn a_call_that_stops_while_its_initializer_runs_hands_over() {
        let cell = Once::new();
        let inits = Inits::default();
        let answers = Channel::bounded(1);
        let cancellation = Cancellation::new();
        let mut first =
            Watched::new(cell.get_or_try_init_until(&cancellation, None, || inits.start(&answers)));
        let mut second = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
        assert!(first.poll().is_pending());
        assert!(second.poll().is_pending());

        cancellation.cancel();
        assert_eq!(first.poll(), Poll::Ready(Err(ScopeError::Cancelled)));
        assert_eq!(inits.dropped(), 1);
        assert_eq!(second.woken(), 1);
        assert!(second.poll().is_pending());
        answers.try_send(Ok(4)).unwrap();
        assert_eq!(second.poll(), Poll::Ready(Ok(&4)));
        assert_eq!(inits.started(), 2);
    }

    #[tokio::test]
    async fn a_call_that_stops_while_it_waits_leaves_the_initializer_running() {
        let cell = Once::new();
        let inits = Inits::default();
        let answers = Channel::bounded(1);
        let cancellation = Cancellation::new();
        let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
        let mut second =
            Watched::new(cell.get_or_try_init_until(&cancellation, None, || inits.start(&answers)));
        assert!(first.poll().is_pending());
        assert!(second.poll().is_pending());

        cancellation.cancel();
        assert_eq!(second.poll(), Poll::Ready(Err(ScopeError::Cancelled)));
        answers.try_send(Ok(1)).unwrap();
        assert_eq!(first.poll(), Poll::Ready(Ok(&1)));
        assert_eq!((inits.started(), inits.dropped()), (1, 1));
    }

    #[cfg(not(target_os = "wasi"))]
    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_passes_while_the_call_waits_ends_only_the_wait() {
        let cell = Once::new();
        let inits = Inits::default();
        let answers = Channel::bounded(1);
        let mut first = Watched::new(cell.get_or_try_init(|| inits.start(&answers)));
        assert!(first.poll().is_pending());
        let deadline = time::now() + std::time::Duration::from_secs(1);
        let result = cell
            .get_or_try_init_until(&Cancellation::new(), Some(deadline), || {
                inits.start(&answers)
            })
            .await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
        assert_eq!(inits.started(), 1);
        answers.try_send(Ok(2)).unwrap();
        assert_eq!(first.poll(), Poll::Ready(Ok(&2)));
    }
}
