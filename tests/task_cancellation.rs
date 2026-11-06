#![cfg(feature = "tokio")]

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

use adamantium::{Cancellation, ScopeError, Tasks};
use tokio::{
    sync::oneshot,
    time::{Instant, timeout},
};

const HUNG: Duration = Duration::from_secs(30);

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn begin<F: Future + Send>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
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
    async fn cancellation_before_start_never_polls_body() {
        let cancel = Cancellation::new();
        cancel.cancel();
        let result = Tasks::<&str>::run_until(cancel, None, async |_| {
            panic!("a cancelled body must not start");
            #[allow(unreachable_code)]
            Ok(())
        }).await;
        assert_eq!(result, Err(ScopeError::Cancelled));
    }

    async fn expired_deadline_never_polls_body() {
        let result = Tasks::<&str>::run_until(
            Cancellation::new(), Some(Instant::now()), async |_| {
                panic!("an expired body must not start");
                #[allow(unreachable_code)]
                Ok(())
            },
        ).await;
        assert_eq!(result, Err(ScopeError::DeadlineExceeded));
    }

    async fn cancellation_wins_over_an_equally_ready_deadline() {
        let cancel = Cancellation::new();
        cancel.cancel();
        assert_eq!(Tasks::<&str>::run_until(cancel, Some(Instant::now()), async |_| {
            Ok(())
        }).await, Err(ScopeError::Cancelled));
    }

    async fn cancellation_drops_body_and_drains_async_children() {
        let cancel = Cancellation::new();
        let body_dropped = Arc::new(AtomicBool::new(false));
        let child_dropped = Arc::new(AtomicBool::new(false));
        let (started, running) = oneshot::channel();
        let mut scope = Box::pin(Tasks::<&str>::run_until(cancel.clone(), None, async |t| {
            let _body = Dropped(body_dropped.clone());
            let guard = Dropped(child_dropped.clone());
            t.spawn(async move {
                let _guard = guard;
                started.send(()).unwrap();
                std::future::pending::<()>().await;
                Ok(())
            });
            std::future::pending::<()>().await;
            Ok(())
        }));
        begin(scope.as_mut()).await;
        timeout(HUNG, running).await.unwrap().unwrap();
        cancel.cancel();
        assert_eq!(timeout(HUNG, scope).await.unwrap(), Err(ScopeError::Cancelled));
        assert!(body_dropped.load(Ordering::SeqCst));
        assert!(child_dropped.load(Ordering::SeqCst));
    }

    async fn child_work_can_wait_for_a_shared_request() {
        let cancel = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        let token = cancel.clone();
        let child = tasks.spawn(async move {
            token.cancelled().await;
            assert!(token.is_cancelled());
            Ok(42)
        });
        cancel.cancel();
        assert_eq!(timeout(HUNG, tasks.value(child)).await.unwrap(), Ok(42));
        tasks.join(Ok(())).await.unwrap();
    }

    async fn all_waiters_and_late_waiters_observe_cancellation() {
        let cancel = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        for _ in 0..16 {
            let token = cancel.clone();
            tasks.spawn(async move {
                token.cancelled().await;
                Ok(())
            });
        }
        let mut waiting = Box::pin(cancel.cancelled());
        begin(waiting.as_mut()).await;
        drop(waiting);
        cancel.cancel();
        cancel.cancel();
        timeout(HUNG, cancel.cancelled()).await.unwrap();
        timeout(HUNG, tasks.join(Ok(()))).await.unwrap().unwrap();
    }

    async fn cancellation_during_join_drains_remaining_children() {
        let cancel = Cancellation::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let mut scope = Box::pin(Tasks::<&str>::run_until(cancel.clone(), None, async |t| {
            t.spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            });
            Ok(42)
        }));
        begin(scope.as_mut()).await;
        cancel.cancel();
        assert_eq!(timeout(HUNG, scope).await.unwrap(), Err(ScopeError::Cancelled));
        assert!(dropped.load(Ordering::SeqCst));
    }

    async fn completed_body_error_wins_over_cancellation() {
        let cancel = Cancellation::new();
        let result = Tasks::run_until(cancel.clone(), None, async |_| {
            cancel.cancel();
            Err::<(), _>("body")
        }).await;
        assert_eq!(result, Err(ScopeError::Failed("body")));
    }

    async fn a_child_error_wins_over_cancellation() {
        let cancel = Cancellation::new();
        let token = cancel.clone();
        let mut tasks = Tasks::new();
        tasks.spawn(async move {
            token.cancel();
            Err::<(), _>("child")
        });
        // Polling the token observes the request without claiming the error.
        timeout(HUNG, cancel.cancelled()).await.unwrap();
        let result = tasks.join_until(cancel, None, Ok(())).await;
        assert_eq!(result, Err(ScopeError::Failed("child")));
    }

    async fn started_blocking_work_is_awaited_and_its_error_wins() {
        let cancel = Cancellation::new();
        let token = cancel.clone();
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let mut tasks = Tasks::new();
        tasks.spawn_blocking(move || {
            let _guard = guard;
            started.send(()).unwrap();
            released.recv_timeout(HUNG).unwrap();
            assert!(token.is_cancelled());
            Err::<(), _>("blocking cleanup")
        });
        timeout(HUNG, running).await.unwrap().unwrap();
        cancel.cancel();
        let mut joined = Box::pin(tasks.join_until(cancel, None, Ok(())));
        begin(joined.as_mut()).await;
        assert!(!dropped.load(Ordering::SeqCst));
        release.send(()).unwrap();
        assert_eq!(timeout(HUNG, joined).await.unwrap(), Err(ScopeError::Failed("blocking cleanup")));
        assert!(dropped.load(Ordering::SeqCst));
    }

    async fn a_body_error_signals_cooperative_blocking_work() {
        let cancel = Cancellation::new();
        let token = cancel.clone();
        let (started, running) = oneshot::channel();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn_blocking(move || {
            started.send(()).unwrap();
            let limit = std::time::Instant::now() + HUNG;
            while !token.is_cancelled() {
                assert!(std::time::Instant::now() < limit, "scope did not signal shutdown");
                std::thread::yield_now();
            }
            Ok(())
        });
        timeout(HUNG, running).await.unwrap().unwrap();
        assert_eq!(timeout(HUNG, tasks.join_until(cancel, None, Err::<(), _>("body"))).await.unwrap(),
            Err(ScopeError::Failed("body")));
    }

    async fn blocking_panic_during_cleanup_wins_and_drains_siblings() {
        let cancel = Cancellation::new();
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Ok(())
        });
        tasks.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(HUNG).unwrap();
            panic!("blocking panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        timeout(HUNG, running).await.unwrap().unwrap();
        cancel.cancel();
        let mut joining = Box::pin(tasks.join_until(cancel, None, Err::<(), _>("body")));
        begin(joining.as_mut()).await;
        release.send(()).unwrap();
        #[allow(clippy::disallowed_methods, reason = "catch the propagated child panic")]
        let joined = tokio::spawn(joining);
        let panic = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"blocking panic"));
        assert!(dropped.load(Ordering::SeqCst));
    }

    async fn dropping_the_scope_signals_but_does_not_wait_for_blocking_work() {
        let cancel = Cancellation::new();
        let token = cancel.clone();
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, complete) = oneshot::channel();
        let mut scope = Box::pin(Tasks::<&str>::run_until(cancel.clone(), None, async |t| {
            t.spawn_blocking(move || {
                started.send(()).unwrap();
                released.recv_timeout(HUNG).unwrap();
                assert!(token.is_cancelled());
                finished.send(()).unwrap();
                Ok(())
            });
            std::future::pending::<()>().await;
            Ok(())
        }));
        begin(scope.as_mut()).await;
        timeout(HUNG, running).await.unwrap().unwrap();
        drop(scope);
        assert!(cancel.is_cancelled());
        release.send(()).unwrap();
        timeout(HUNG, complete).await.unwrap().unwrap();
    }

    async fn dropping_an_unpolled_join_signals_and_aborts() {
        let cancel = Cancellation::new();
        let (held, dropped) = oneshot::channel::<()>();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
            Ok(())
        });
        drop(tasks.join_until(cancel.clone(), None, Ok(())));
        assert!(cancel.is_cancelled());
        assert!(timeout(HUNG, dropped).await.unwrap().is_err());
    }

    async fn successful_scope_preserves_values_and_claimed_errors() {
        let cancel = Cancellation::new();
        let result = Tasks::run_until(cancel.clone(), None, async |t| {
            let failed = t.spawn(async { Err::<(), _>("handled") });
            assert_eq!(t.value(failed).await, Err("handled"));
            let value = t.spawn_blocking(|| Ok(42));
            t.value(value).await
        }).await;
        assert_eq!(result, Ok(42));
        assert!(cancel.is_cancelled());
    }

    async fn cancellation_cannot_discard_a_panic_already_being_drained() {
        for use_value in [false, true] {
            let cancel = Cancellation::new();
            let (started, running) = oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let (fail, failure) = oneshot::channel();
            let mut scope = Box::pin(Tasks::<&str>::run_until(cancel.clone(), None, async move |t| {
                t.spawn_blocking(move || {
                    started.send(()).unwrap();
                    released.recv_timeout(HUNG).unwrap();
                    Ok(())
                });
                let panicking = t.spawn(async move {
                    failure.await.unwrap();
                    panic!("retained panic");
                    #[allow(unreachable_code)]
                    Ok(())
                });
                if use_value {
                    t.value(panicking).await?;
                }
                Ok(())
            }));
            begin(scope.as_mut()).await;
            timeout(HUNG, running).await.unwrap().unwrap();
            fail.send(()).unwrap();
            // The panic path signals shutdown before waiting for the blocking
            // sibling. Poll again so cancellation interrupts that drain.
            timeout(HUNG, std::future::poll_fn(|cx| {
                assert!(scope.as_mut().poll(cx).is_pending());
                if cancel.is_cancelled() { Poll::Ready(()) } else { Poll::Pending }
            })).await.unwrap();
            begin(scope.as_mut()).await;
            release.send(()).unwrap();
            #[allow(clippy::disallowed_methods, reason = "catch the propagated child panic")]
            let joined = tokio::spawn(scope);
            let panic = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"retained panic"));
        }
    }

    async fn body_panic_signals_shutdown_without_waiting_for_blocking_work() {
        let cancel = Cancellation::new();
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, complete) = oneshot::channel();
        let scope = Tasks::<&str>::run_until(cancel.clone(), None, async move |t| {
            t.spawn_blocking(move || {
                started.send(()).unwrap();
                released.recv_timeout(HUNG).unwrap();
                finished.send(()).unwrap();
                Ok(())
            });
            running.await.unwrap();
            panic!("body panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        #[allow(clippy::disallowed_methods, reason = "catch the body panic")]
        let joined = tokio::spawn(scope);
        let panic = timeout(HUNG, joined).await.unwrap().unwrap_err().into_panic();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"body panic"));
        assert!(cancel.is_cancelled());
        release.send(()).unwrap();
        timeout(HUNG, complete).await.unwrap().unwrap();
    }
}

#[test]
fn cancellation_prevents_queued_blocking_work_from_starting() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cancel = Cancellation::new();
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let queued_ran = Arc::new(AtomicBool::new(false));
        let ran = queued_ran.clone();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(HUNG).unwrap();
            Ok(())
        });
        timeout(HUNG, running).await.unwrap().unwrap();
        tasks.spawn_blocking(move || {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        });
        cancel.cancel();
        let mut joined = Box::pin(tasks.join_until(cancel, None, Ok(())));
        begin(joined.as_mut()).await;
        release.send(()).unwrap();
        assert_eq!(
            timeout(HUNG, joined).await.unwrap(),
            Err(ScopeError::Cancelled)
        );
        assert!(!queued_ran.load(Ordering::SeqCst));
    });
}

#[tokio::test(start_paused = true)]
async fn deadline_interrupts_body_and_drains_children() {
    let cancel = Cancellation::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut scope = Box::pin(Tasks::<&str>::run_until(
        cancel.clone(),
        Some(deadline),
        async |t| {
            t.spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            });
            std::future::pending::<()>().await;
            Ok(())
        },
    ));
    begin(scope.as_mut()).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(scope.await, Err(ScopeError::DeadlineExceeded));
    assert!(cancel.is_cancelled());
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn deadline_during_join_waits_for_blocking_cleanup_and_keeps_its_reason() {
    let cancel = Cancellation::new();
    let (started, running) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut tasks = Tasks::<&str>::new();
    tasks.spawn_blocking(move || {
        started.send(()).unwrap();
        released.recv_timeout(HUNG).unwrap();
        Ok(())
    });
    // Avoid a virtual-time timeout while waiting for an actual OS thread.
    running.await.unwrap();
    let mut joined = Box::pin(tasks.join_until(cancel.clone(), Some(deadline), Ok(())));
    begin(joined.as_mut()).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    begin(joined.as_mut()).await;
    assert!(cancel.is_cancelled());
    cancel.cancel();
    tokio::time::advance(Duration::from_secs(20)).await;
    begin(joined.as_mut()).await;
    release.send(()).unwrap();
    assert_eq!(joined.await, Err(ScopeError::DeadlineExceeded));
}
