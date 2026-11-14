#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "integration proof for the private scheduling engine"
)]

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;
    use shuttle::{
        future::{block_on, spawn},
        sync::Mutex,
    };
    use std::{
        cell::Cell,
        future::{Future, pending},
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    thread_local! { static EXECUTION: Cell<bool> = const { Cell::new(false) }; }
    shuttle::thread_local! { static INSIDE: Cell<bool> = const { Cell::new(false) }; }

    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn check(f: impl Fn() + Send + Sync + 'static) {
        shuttle::check_random_with_seed(f, 321, 100);
    }

    #[test]
    fn execution_mark_is_shared_but_nesting_is_task_local() {
        EXECUTION.set(true);
        let os_thread = std::thread::current().id();
        check(move || {
            block_on(async move {
                let lock = Arc::new(Mutex::new(0));
                let mut handles = Vec::new();
                for _ in 0..3 {
                    let lock = lock.clone();
                    handles.push(spawn(async move {
                        assert_eq!(std::thread::current().id(), os_thread);
                        assert!(EXECUTION.get());
                        INSIDE.with(|inside| {
                            assert!(!inside.replace(true));
                        });
                        *lock.lock().unwrap() += 1;
                        INSIDE.with(|inside| {
                            assert!(inside.replace(false));
                        });
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
                assert_eq!(*lock.lock().unwrap(), 3);
                INSIDE.with(|inside| assert!(!inside.get()));
            })
        });
        EXECUTION.set(false);
    }

    #[test]
    fn catch_child_panic_and_recover_poison_without_stopping_siblings() {
        check(|| {
            block_on(async {
                let dropped = Arc::new(AtomicUsize::new(0));
                let lock = Arc::new(Mutex::new(0));
                let child_lock = lock.clone();
                let child_drop = Dropped(dropped.clone());
                let child = spawn(
                    AssertUnwindSafe(async move {
                        let _drop = child_drop;
                        let mut state = child_lock.lock().unwrap();
                        *state = 7;
                        panic!("child panic");
                    })
                    .catch_unwind(),
                );
                assert!(child.await.unwrap().is_err());
                assert_eq!(dropped.load(Ordering::SeqCst), 1);
                assert_eq!(*lock.lock().unwrap_or_else(|e| e.into_inner()), 7);
            })
        });
    }

    #[test]
    fn abort_drops_future_before_join_both_started_and_queued() {
        check(|| {
            block_on(async {
                let dropped = Arc::new(AtomicUsize::new(0));
                let guard = Dropped(dropped.clone());
                let child = spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                });
                child.abort();
                assert!(child.await.is_err());
                assert_eq!(dropped.load(Ordering::SeqCst), 1);
            })
        });
    }

    #[test]
    fn dropped_scope_abort_handles_still_allow_drain() {
        struct Scope(Vec<shuttle::future::AbortHandle>);
        impl Drop for Scope {
            fn drop(&mut self) {
                for handle in &self.0 {
                    handle.abort();
                }
            }
        }
        check(|| {
            block_on(async {
                let dropped = Arc::new(AtomicUsize::new(0));
                let guard = Dropped(dropped.clone());
                let task = spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                });
                let scope = Scope(vec![task.abort_handle()]);
                drop(scope);
                assert!(task.await.is_err());
                assert_eq!(dropped.load(Ordering::SeqCst), 1);
            })
        });
    }

    // A queued/started gate models spawn_blocking: abort may win before the
    // synchronous closure enters, but a started closure is joined to completion.
    #[test]
    fn blocking_gate_preserves_started_work() {
        check(|| {
            #[derive(Default)]
            struct State {
                started: bool,
                aborted: bool,
                finished: bool,
            }
            let state = Arc::new(Mutex::new(State::default()));
            let child_state = state.clone();
            let child = shuttle::thread::spawn(move || {
                {
                    let mut state = child_state.lock().unwrap();
                    if state.aborted {
                        return;
                    }
                    state.started = true;
                }
                shuttle::thread::yield_now();
                child_state.lock().unwrap().finished = true;
            });
            state.lock().unwrap().aborted = true;
            child.join().unwrap();
            let state = state.lock().unwrap();
            assert_eq!(state.started, state.finished);
        });
    }

    // A cancelled future can itself panic in Drop. Catching poll alone does not
    // catch that panic; the adapter must explicitly catch destruction too.
    #[test]
    fn panic_on_cancelled_future_drop_can_be_transported() {
        struct PanicDrop;
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                panic!("drop panic");
            }
        }
        struct CatchDrop<F: Future> {
            future: Option<std::pin::Pin<Box<F>>>,
            panic: Arc<Mutex<bool>>,
        }
        impl<F: Future> Future for CatchDrop<F> {
            type Output = F::Output;
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                self.future.as_mut().unwrap().as_mut().poll(cx)
            }
        }
        impl<F: Future> Drop for CatchDrop<F> {
            fn drop(&mut self) {
                if catch_unwind(AssertUnwindSafe(|| drop(self.future.take()))).is_err() {
                    *self.panic.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
            }
        }
        check(|| {
            block_on(async {
                let panic = Arc::new(Mutex::new(false));
                let guard = PanicDrop;
                let task = spawn(CatchDrop {
                    future: Some(Box::pin(async move {
                        let _guard = guard;
                        pending::<()>().await;
                    })),
                    panic: panic.clone(),
                });
                task.abort();
                assert!(task.await.is_err());
                assert!(*panic.lock().unwrap());
            })
        });
    }
}
