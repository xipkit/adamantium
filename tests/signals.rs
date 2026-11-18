#![cfg(all(feature = "signal", any(unix, windows)))]

mod support;

use std::{pin::pin, task::Poll, time::Duration};

use adamantium::{Cancellation, Tasks};
use support::timeout;

const HUNG: Duration = Duration::from_secs(10);

async fn begin<F: Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn already_cancelled_returns_immediately() {
    let cancellation = Cancellation::new();
    cancellation.cancel();
    timeout(HUNG, cancellation.cancel_on_signal())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn manual_cancellation_finishes_listener() {
    let cancellation = Cancellation::new();
    let mut listener = pin!(cancellation.cancel_on_signal());
    begin(listener.as_mut()).await;
    cancellation.clone().cancel();
    timeout(HUNG, listener).await.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_listener_does_not_cancel() {
    let cancellation = Cancellation::new();
    {
        let mut listener = pin!(cancellation.cancel_on_signal());
        begin(listener.as_mut()).await;
    }
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
async fn scope_completion_finishes_listener() {
    let cancellation = Cancellation::new();
    let (_, outcome) = timeout(HUNG, async {
        tokio::try_join!(cancellation.cancel_on_signal(), async {
            Ok::<_, std::io::Error>(
                Tasks::<std::io::Error>::run_until(cancellation.clone(), None, async |_| Ok(42))
                    .await,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.unwrap(), 42);
}

#[cfg(unix)]
mod unix {
    use super::*;
    use adamantium::{Reply, ScopeError};
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn signal_child() {
        let Ok(runtime) = std::env::var("ADAMANTIUM_SIGNAL_TEST_RUNTIME") else {
            return;
        };
        let mut runtime = if runtime == "current_thread" {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        runtime.enable_all().build().unwrap().block_on(async {
            let cancellation = Cancellation::new();
            let observer = cancellation.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let mut listener = pin!(cancellation.cancel_on_signal());
            begin(listener.as_mut()).await;
            let scope = Tasks::<std::io::Error>::run_until(observer.clone(), None, async |tasks| {
                let guard = Dropped(dropped.clone());
                let (started, running) = Reply::pair();
                tasks.spawn(async move {
                    let _guard = guard;
                    started.send(()).unwrap();
                    std::future::pending::<Result<(), std::io::Error>>().await
                });
                running.receive().await.unwrap();
                println!("SIGNAL_LISTENER_READY");
                std::io::stdout().flush().unwrap();
                std::future::pending::<Result<(), std::io::Error>>().await
            });
            let (_, outcome) = timeout(HUNG, async {
                tokio::try_join!(listener, async { Ok::<_, std::io::Error>(scope.await) })
            })
            .await
            .unwrap()
            .unwrap();
            assert!(matches!(outcome, Err(ScopeError::Cancelled)));
            assert!(observer.is_cancelled());
            assert!(dropped.load(Ordering::SeqCst));
            observer.cancelled().await;
        });
    }

    #[test]
    fn process_signals_cancel_and_drain_scope() {
        for runtime in ["current_thread", "multi_thread"] {
            for signal in ["INT", "TERM"] {
                let mut child = ChildGuard(
                    Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "unix::signal_child", "--nocapture"])
                        .env("ADAMANTIUM_SIGNAL_TEST_RUNTIME", runtime)
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap(),
                );
                let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
                let mut ready = false;
                for line in (&mut stdout).lines() {
                    if line.unwrap().contains("SIGNAL_LISTENER_READY") {
                        ready = true;
                        break;
                    }
                }
                assert!(ready, "listener did not start on {runtime}");
                assert!(
                    Command::new("kill")
                        .args(["-s", signal, &child.0.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
                assert!(child.0.wait().unwrap().success(), "{signal} on {runtime}");
            }
        }
    }
}
