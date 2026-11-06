#![cfg(feature = "tokio")]

use std::{
    future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

use adamantium::{
    Cancellation, ScopeError, concurrently_ordered, concurrently_ordered_until,
    concurrently_unordered, concurrently_unordered_until,
};
use futures_core::Stream;
use futures_util::StreamExt;
use tokio::sync::{Semaphore, oneshot};
use tokio::time::timeout;

const HUNG: Duration = Duration::from_secs(30);

type Results<'a, T> = Pin<Box<dyn Stream<Item = Result<T, ScopeError<&'static str>>> + Send + 'a>>;

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Counted {
    live: Arc<AtomicUsize>,
}

impl Counted {
    fn new(live: &Arc<AtomicUsize>, peak: &AtomicUsize) -> Self {
        let count = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(count, Ordering::SeqCst);
        Self { live: live.clone() }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn pending<S: Stream + Send>(mut stream: Pin<&mut S>) {
    future::poll_fn(|cx| {
        assert!(stream.as_mut().poll_next(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

macro_rules! on_both_runtimes {
    ($(async fn $name:ident() $body:block)*) => {$(
        mod $name {
            use super::*;

            #[tokio::test]
            async fn current_thread() $body

            #[tokio::test(flavor = "multi_thread")]
            async fn multi_thread() $body
        }
    )*};
}

on_both_runtimes! {
    async fn ordered_slow_head_bounds_work_and_results() {
        let taken = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(Semaphore::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let cancellation = Cancellation::new();
        let mut stream = Box::pin(concurrently_ordered_until(0..1_000_000, 8, cancellation.clone(), |n| {
            taken.fetch_add(1, Ordering::SeqCst);
            let value = Counted::new(&live, &peak);
            let gate = gate.clone();
            let completed = completed.clone();
            async move {
                if n == 0 {
                    let _permit = gate.acquire().await.unwrap();
                } else {
                    completed.add_permits(1);
                }
                Ok::<_, &str>(value)
            }
        }));
        pending(stream.as_mut()).await;
        let _finished = timeout(HUNG, completed.acquire_many(7)).await.unwrap().unwrap();
        pending(stream.as_mut()).await;
        assert_eq!(taken.load(Ordering::SeqCst), 8);
        assert_eq!(live.load(Ordering::SeqCst), 8);
        assert_eq!(peak.load(Ordering::SeqCst), 8);
        cancellation.cancel();
        assert!(matches!(timeout(HUNG, stream.next()).await.unwrap(), Some(Err(ScopeError::Cancelled))));
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(stream.next().await.is_none());
    }

    async fn ordered_results_wait_for_head_then_keep_input_order() {
        let gate = Arc::new(Semaphore::new(0));
        let completed = Arc::new(Semaphore::new(0));
        let mut stream = Box::pin(concurrently_ordered(0..20, 4, |n| {
            let gate = gate.clone();
            let completed = completed.clone();
            async move {
                if n == 0 { let _permit = gate.acquire().await.unwrap(); }
                else { completed.add_permits(1); }
                Ok::<_, &str>(n)
            }
        }));
        pending(stream.as_mut()).await;
        let _finished = timeout(HUNG, completed.acquire_many(3)).await.unwrap().unwrap();
        pending(stream.as_mut()).await;
        gate.add_permits(1);
        for expected in 0..20 {
            assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Ok(expected)));
        }
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.next().await, None);
    }

    async fn unordered_yields_later_items_and_refills_only_when_polled() {
        let gates: Vec<_> = (0..3).map(|_| Arc::new(Semaphore::new(0))).collect();
        let taken = Arc::new(AtomicUsize::new(0));
        let mut stream = Box::pin(concurrently_unordered(gates.iter().cloned().enumerate(), 2, |(n, gate)| {
            taken.fetch_add(1, Ordering::SeqCst);
            async move {
                let _permit = gate.acquire().await.unwrap();
                Ok::<_, &str>(n)
            }
        }));
        pending(stream.as_mut()).await;
        gates.get(1).unwrap().add_permits(1);
        assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Ok(1)));
        assert_eq!(taken.load(Ordering::SeqCst), 2);
        pending(stream.as_mut()).await;
        assert_eq!(taken.load(Ordering::SeqCst), 3);
        gates.get(2).unwrap().add_permits(1);
        assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Ok(2)));
        gates.first().unwrap().add_permits(1);
        assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Ok(0)));
        assert_eq!(stream.next().await, None);
    }

    async fn slow_consumer_bounds_both_orders() {
        for ordered in [true, false] {
            let taken = Arc::new(AtomicUsize::new(0));
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(Semaphore::new(0));
            let f = |_| {
                taken.fetch_add(1, Ordering::SeqCst);
                let value = Counted::new(&live, &peak);
                let completed = completed.clone();
                async move {
                    completed.add_permits(1);
                    Ok::<_, &str>(value)
                }
            };
            let mut stream: Results<'_, Counted> = if ordered {
                Box::pin(concurrently_ordered(0..1_000_000, 8, f))
            } else {
                Box::pin(concurrently_unordered(0..1_000_000, 8, f))
            };
            drop(timeout(HUNG, stream.next()).await.unwrap().unwrap().unwrap());
            let _finished = timeout(HUNG, completed.acquire_many(8)).await.unwrap().unwrap();
            // Leave the consumer paused while all work in its window finishes.
            for _ in 0..10 { tokio::task::yield_now().await; }
            assert_eq!(taken.load(Ordering::SeqCst), 8);
            assert_eq!(live.load(Ordering::SeqCst), 7);
            assert_eq!(peak.load(Ordering::SeqCst), 8);
            drop(stream);
            timeout(HUNG, async {
                while live.load(Ordering::SeqCst) != 0 { tokio::task::yield_now().await; }
            }).await.unwrap();
        }
    }

    async fn later_error_bypasses_slow_head_and_drains() {
        for ordered in [true, false] {
            let taken = Arc::new(AtomicUsize::new(0));
            let dropped = Arc::new(AtomicUsize::new(0));
            let f = |n| {
                taken.fetch_add(1, Ordering::SeqCst);
                let guard = Dropped(dropped.clone());
                async move {
                    let _guard = guard;
                    if n == 1 { Err("later failure") }
                    else { future::pending::<Result<(), &str>>().await }
                }
            };
            let mut stream: Results<'_, ()> = if ordered {
                Box::pin(concurrently_ordered(0..1_000_000, 2, f))
            } else {
                Box::pin(concurrently_unordered(0..1_000_000, 2, f))
            };
            assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Err(ScopeError::Failed("later failure"))));
            assert_eq!(taken.load(Ordering::SeqCst), 2);
            assert_eq!(dropped.load(Ordering::SeqCst), 2);
            assert_eq!(stream.next().await, None);
        }
    }

    async fn later_panic_drains_before_resuming() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let count = dropped.clone();
        let stream = concurrently_ordered(0..3, 2, move |n| {
            let guard = Dropped(count.clone());
            async move {
                let _guard = guard;
                if n == 1 { panic!("stream child panic"); }
                future::pending::<Result<(), &str>>().await
            }
        });
        #[allow(clippy::disallowed_methods, reason = "catch the stream's resumed panic")]
        let caught = tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            stream.next().await
        });
        let error = timeout(HUNG, caught).await.unwrap().unwrap_err();
        assert_eq!(error.into_panic().downcast_ref::<&str>(), Some(&"stream child panic"));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    async fn cancellation_before_start_and_during_pending_work() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let mut stream = Box::pin(concurrently_ordered_until(0..100, 8, cancellation.clone(), |_| {
            panic!("cancelled stream took an item");
            #[allow(unreachable_code)]
            async { Ok::<_, &str>(()) }
        }));
        assert_eq!(stream.next().await, Some(Err(ScopeError::Cancelled)));
        assert_eq!(stream.next().await, None);

        let cancellation = Cancellation::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let (cancel, cancelled) = oneshot::channel();
        let mut cancel = Some(cancel);
        let mut stream = Box::pin(concurrently_unordered_until(0..100, 2, cancellation.clone(), |_| {
            let guard = Dropped(dropped.clone());
            let cancel = cancel.take();
            let cancellation = cancellation.clone();
            async move {
                let _guard = guard;
                if let Some(cancel) = cancel {
                    let _ = cancel.send(cancellation);
                }
                future::pending::<Result<(), &str>>().await
            }
        }));
        pending(stream.as_mut()).await;
        timeout(HUNG, cancelled).await.unwrap().unwrap().cancel();
        assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Err(ScopeError::Cancelled)));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert_eq!(stream.next().await, None);
    }

    async fn completion_failure_and_drop_do_not_cancel_parent_or_sibling() {
        let parent = Cancellation::new();
        let gate = Arc::new(Semaphore::new(0));
        let mut sibling = Box::pin(concurrently_ordered_until(0..1, 1, parent.clone(), |_| {
            let gate = gate.clone();
            async move {
                let _permit = gate.acquire().await.unwrap();
                Ok::<_, &str>(42)
            }
        }));
        pending(sibling.as_mut()).await;
        {
            let mut success = Box::pin(concurrently_ordered_until(0..1, 1, parent.clone(), |_| async { Ok::<_, &str>(1) }));
            assert_eq!(success.next().await, Some(Ok(1)));
            assert_eq!(success.next().await, None);
        }
        {
            let mut failure = Box::pin(concurrently_unordered_until(0..1, 1, parent.clone(), |_| async { Err::<(), _>("error") }));
            assert_eq!(failure.next().await, Some(Err(ScopeError::Failed("error"))));
        }
        {
            let mut abandoned = Box::pin(concurrently_ordered_until(0..1, 1, parent.clone(), |_| future::pending::<Result<(), &str>>()));
            pending(abandoned.as_mut()).await;
        }
        assert!(!parent.is_cancelled());
        gate.add_permits(1);
        assert_eq!(timeout(HUNG, sibling.next()).await.unwrap(), Some(Ok(42)));
        assert_eq!(sibling.next().await, None);
        assert!(!parent.is_cancelled());
    }

    async fn dropping_next_preserves_work_and_dropping_stream_aborts() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let mut stream = Box::pin(concurrently_ordered(0..2, 2, |n| {
            let guard = Dropped(dropped.clone());
            let gate = gate.clone();
            async move {
                let _guard = guard;
                if n == 0 {
                    let _permit = gate.acquire().await.unwrap();
                    Ok::<_, &str>(n)
                } else { future::pending().await }
            }
        }));
        // A select loser drops next(), but must not cancel or discard its task.
        tokio::select! {
            biased;
            _ = stream.next() => panic!("not ready"),
            () = future::ready(()) => {}
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        gate.add_permits(1);
        assert_eq!(timeout(HUNG, stream.next()).await.unwrap(), Some(Ok(0)));
        drop(stream);
        timeout(HUNG, async {
            while dropped.load(Ordering::SeqCst) != 2 { tokio::task::yield_now().await; }
        }).await.unwrap();
    }

    async fn large_input_has_constant_live_work_and_result_storage() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut stream = Box::pin(concurrently_ordered(0..100_000, 8, |n| {
            let value = Counted::new(&live, &peak);
            async move { Ok::<_, &str>((n, value)) }
        }));
        for expected in 0..100_000 {
            let (n, value) = timeout(HUNG, stream.next()).await.unwrap().unwrap().unwrap();
            assert_eq!(n, expected);
            drop(value);
        }
        assert!(stream.next().await.is_none());
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(peak.load(Ordering::SeqCst) <= 8);
    }

    async fn zero_limit_empty_input_and_non_fused_input() {
        let mut zero = Box::pin(concurrently_ordered(0..4, 0, |n| async move { Ok::<_, &str>(n) }));
        for n in 0..4 { assert_eq!(zero.next().await, Some(Ok(n))); }
        assert_eq!(zero.next().await, None);
        let mut empty = Box::pin(concurrently_unordered(0..0, 0, |n| async move { Ok::<_, &str>(n) }));
        assert_eq!(empty.next().await, None);
        let mut values = [Some(0), None, Some(2)].into_iter();
        let iter = std::iter::from_fn(move || values.next().unwrap());
        let mut stream = Box::pin(concurrently_ordered(iter, 8, |n| async move { Ok::<_, &str>(n) }));
        assert_eq!(stream.next().await, Some(Ok(0)));
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.next().await, None);
    }
}

#[test]
fn construction_is_lazy_and_needs_no_runtime() {
    let iter = std::iter::from_fn(|| -> Option<()> {
        panic!("must not take an item");
    });
    let stream = concurrently_ordered(iter, 2, |_| {
        panic!("must not construct a future");
        #[allow(unreachable_code)]
        async {
            Ok::<_, &str>(())
        }
    });
    drop(stream);
}

#[tokio::test]
async fn non_send_items_and_borrowed_closure_are_supported() {
    let offset = 7;
    #[allow(
        clippy::disallowed_types,
        reason = "prove that input items need not be Send"
    )]
    let items = (0..3).map(std::rc::Rc::new);
    let mut stream = Box::pin(concurrently_ordered(items, 2, |item| {
        let value = *item + offset;
        async move { Ok::<_, &str>(value) }
    }));
    for n in 7..10 {
        assert_eq!(stream.next().await, Some(Ok(n)));
    }
    assert_eq!(stream.next().await, None);
}

#[tokio::test]
async fn recorded_error_wins_over_cancellation_and_buffered_success() {
    let cancel = Cancellation::new();
    let mut stream = Box::pin(concurrently_ordered_until(
        0..3,
        3,
        cancel.clone(),
        |n| async move {
            match n {
                0 => future::pending().await,
                1 => Ok(1),
                _ => Err("recorded error"),
            }
        },
    ));
    pending(stream.as_mut()).await;
    // On this current-thread runtime the children complete during this yield,
    // while the consumer remains paused and cannot begin cleanup yet.
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(
        stream.next().await,
        Some(Err(ScopeError::Failed("recorded error")))
    );
    assert_eq!(stream.next().await, None);
}

#[tokio::test]
async fn recorded_panic_wins_over_error_and_cancellation() {
    let cancel = Cancellation::new();
    let parent = cancel.clone();
    let stream = concurrently_ordered_until(0..3, 3, cancel.clone(), |n| async move {
        match n {
            0 => future::pending().await,
            1 => Err::<(), _>("error"),
            _ => panic!("panic wins"),
        }
    });
    let mut stream = Box::pin(stream);
    pending(stream.as_mut()).await;
    tokio::task::yield_now().await;
    cancel.cancel();
    #[allow(
        clippy::disallowed_methods,
        reason = "catch the stream's resumed panic"
    )]
    let caught = tokio::spawn(async move { stream.next().await });
    let error = timeout(HUNG, caught).await.unwrap().unwrap_err();
    assert_eq!(
        error.into_panic().downcast_ref::<&str>(),
        Some(&"panic wins")
    );
    assert!(parent.is_cancelled());
}

#[tokio::test]
async fn oldest_error_wins_even_when_input_order_differs() {
    let (send, receive) = oneshot::channel();
    let mut send = Some(send);
    let mut receive = Some(receive);
    let mut stream = Box::pin(concurrently_ordered(0..3, 3, |n| {
        let send = if n == 2 { send.take() } else { None };
        let receive = if n == 1 { receive.take() } else { None };
        async move {
            if let Some(receive) = receive {
                receive.await.unwrap();
                Err("later error at index 1")
            } else if let Some(send) = send {
                send.send(()).unwrap();
                Err("first error at index 2")
            } else {
                future::pending::<Result<(), &str>>().await
            }
        }
    }));
    pending(stream.as_mut()).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(
        stream.next().await,
        Some(Err(ScopeError::Failed("first error at index 2")))
    );
    assert_eq!(stream.next().await, None);
}

#[tokio::test]
async fn cancellation_discards_ready_successes_before_refilling() {
    let cancel = Cancellation::new();
    let taken = Arc::new(AtomicUsize::new(0));
    let mut stream = Box::pin(concurrently_unordered_until(
        0..100,
        2,
        cancel.clone(),
        |n| {
            taken.fetch_add(1, Ordering::SeqCst);
            async move { Ok::<_, &str>(n) }
        },
    ));
    assert!(stream.next().await.unwrap().is_ok());
    cancel.cancel();
    assert_eq!(stream.next().await, Some(Err(ScopeError::Cancelled)));
    assert_eq!(taken.load(Ordering::SeqCst), 2);
    assert_eq!(stream.next().await, None);
}

#[tokio::test]
async fn synchronous_panics_abort_and_leave_parent_uncancelled() {
    for iterator_panics in [true, false] {
        let parent = Cancellation::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let count = dropped.clone();
        let items = (0..3).inspect(move |n| {
            if iterator_panics && *n == 1 {
                panic!("iterator panic");
            }
        });
        let stream = concurrently_ordered_until(items, 2, parent.clone(), move |n| {
            if n == 1 {
                panic!("closure panic");
            }
            let guard = Dropped(count.clone());
            async move {
                let _guard = guard;
                future::pending::<Result<(), &str>>().await
            }
        });
        #[allow(clippy::disallowed_methods, reason = "catch synchronous poll panic")]
        let caught = tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            stream.next().await
        });
        let error = timeout(HUNG, caught).await.unwrap().unwrap_err();
        assert!(error.is_panic());
        timeout(HUNG, async {
            while dropped.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!parent.is_cancelled());
    }
}

#[test]
fn runtime_shutdown_panics_without_taking_more_items() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    };
    let called = Arc::new(AtomicUsize::new(0));
    let count = called.clone();
    let mut stream = Box::pin(concurrently_ordered(0..3, 2, move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        future::pending::<Result<(), &str>>()
    }));
    let first = runtime();
    first.block_on(pending(stream.as_mut()));
    drop(first);
    #[allow(
        clippy::disallowed_methods,
        reason = "catch panic after the spawning runtime shuts down"
    )]
    let caught = runtime()
        .block_on(async { timeout(HUNG, tokio::spawn(async move { stream.next().await })).await });
    let error = caught.unwrap().unwrap_err();
    assert_eq!(
        error.into_panic().downcast_ref::<&str>(),
        Some(&"adamantium: a task ended without a result and without a panic")
    );
    assert_eq!(called.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn exhausted_cooperative_budget_pauses_intake_and_preserves_results() {
    let taken = Arc::new(AtomicUsize::new(0));
    let mut stream = Box::pin(concurrently_ordered(0..100, 2, |n| {
        taken.fetch_add(1, Ordering::SeqCst);
        async move { Ok::<_, &str>(n) }
    }));
    pending(stream.as_mut()).await;
    tokio::task::yield_now().await;
    future::poll_fn(|cx| {
        while tokio::task::coop::has_budget_remaining() {
            let Poll::Ready(budget) = tokio::task::coop::poll_proceed(cx) else {
                panic!("budget was available");
            };
            budget.made_progress();
        }
        // Even ready results wait for a fresh budget. Do not bypass Tokio's
        // cooperative yield by refilling or emitting buffered values here.
        assert!(stream.as_mut().poll_next(cx).is_pending());
        assert_eq!(taken.load(Ordering::SeqCst), 2);
        Poll::Ready(())
    })
    .await;
    for n in 0..100 {
        assert_eq!(stream.next().await, Some(Ok(n)));
    }
    assert_eq!(stream.next().await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_waits_for_synchronous_destructors() {
    struct Cleanup {
        started: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.started.take().unwrap().send(());
            self.release.recv_timeout(HUNG).unwrap();
        }
    }
    let cancel = Cancellation::new();
    let (started, cleaning) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let mut guard = Some(Cleanup {
        started: Some(started),
        release: wait,
    });
    let mut stream = Box::pin(concurrently_ordered_until(
        0..1,
        1,
        cancel.clone(),
        move |_| {
            let guard = guard.take().unwrap();
            async move {
                let _guard = guard;
                future::pending::<Result<(), &str>>().await
            }
        },
    ));
    pending(stream.as_mut()).await;
    cancel.cancel();
    let (sent, received) = oneshot::channel();
    #[allow(
        clippy::disallowed_methods,
        reason = "observe the consumer independently while cleanup is blocked"
    )]
    let consumer = tokio::spawn(async move {
        let result = stream.next().await;
        sent.send(result).unwrap();
        stream
    });
    timeout(HUNG, cleaning).await.unwrap().unwrap();
    let mut received = Box::pin(received);
    future::poll_fn(|cx| {
        assert!(
            received.as_mut().poll(cx).is_pending(),
            "terminal error preceded cleanup"
        );
        Poll::Ready(())
    })
    .await;
    release.send(()).unwrap();
    assert_eq!(
        timeout(HUNG, received).await.unwrap().unwrap(),
        Some(Err(ScopeError::Cancelled))
    );
    let mut stream = timeout(HUNG, consumer).await.unwrap().unwrap();
    assert_eq!(stream.next().await, None);
}
