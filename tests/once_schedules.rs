#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    reason = "observations collected across executions"
)]

use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{Arc, Mutex},
    task::Poll,
};

use adamantium::{
    Actor, Cancellation, Channel, Once, ScopeError,
    testing::{self, Config},
};

fn config() -> Config {
    Config {
        seed: 254,
        iterations: 300,
        max_steps: 20_000,
        ..Config::default()
    }
}

fn check<F, Fut>(factory: F)
where
    F: Fn() -> Fut + Send + Sync + 'static + Clone,
    Fut: Future<Output = ()>,
{
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

// Steps an initializer takes, so other calls run while it does.
const INIT_STEPS: usize = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Log {
    started: u32,
    finished: u32,
    dropped: u32,
    running: bool,
}

#[derive(Clone)]
struct Service {
    connection: Once<u32>,
    log: Actor<Log>,
    joined: Channel<()>,
}

impl Service {
    fn new(callers: usize) -> Self {
        Self {
            connection: Once::new(),
            log: Actor::default(),
            joined: Channel::bounded(callers),
        }
    }

    // Fails its first attempt once `callers` calls have joined, if `fail_first`.
    async fn connect(self, fail_first: bool, callers: usize) -> Result<u32, String> {
        let attempt = self.log.with(|log| {
            assert!(!log.running, "two initializers ran at once");
            log.running = true;
            log.started += 1;
            log.started
        });
        let mut running = Running {
            log: self.log.clone(),
            finished: false,
        };
        if attempt == 1 {
            for _ in 0..callers {
                self.joined.receive().await;
            }
        }
        // Each step waits, as for a reply, so a call can stop in the middle.
        for _ in 0..INIT_STEPS {
            self.log.read(|_| ());
            yield_once().await;
        }
        running.finished = true;
        if fail_first && attempt == 1 {
            return Err("down".into());
        }
        Ok(42)
    }

    fn log(&self) -> Log {
        self.log.snapshot()
    }
}

async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
        if std::mem::replace(&mut yielded, true) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

struct Running {
    log: Actor<Log>,
    finished: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        let finished = self.finished;
        self.log.with(|log| {
            log.running = false;
            if finished {
                log.finished += 1;
            } else {
                log.dropped += 1;
            }
        });
    }
}

// Reports on `joined` once the call's first poll has returned, so it is
// waiting or initializing.
async fn joining<F: Future>(call: F, joined: &Channel<()>) -> F::Output {
    let mut call = pin!(call);
    let mut first = true;
    poll_fn(|cx| {
        let polled = call.as_mut().poll(cx);
        if std::mem::take(&mut first) {
            joined.try_send(()).unwrap();
        }
        polled
    })
    .await
}

#[test]
fn concurrent_first_calls_run_one_initializer_and_share_its_value() {
    check(|| async {
        let service = Service::new(3);
        let workers = (0..3).map(|_| {
            let service = service.clone();
            async move {
                let call = service
                    .connection
                    .get_or_try_init(|| service.clone().connect(false, 3));
                joining(call, &service.joined).await.copied()
            }
        });
        let results = testing::parallel(workers).await;
        assert_eq!(results, [Ok(42), Ok(42), Ok(42)]);
        let log = service.log();
        assert_eq!((log.started, log.finished, log.dropped), (1, 1, 0));
        assert_eq!(service.connection.get(), Some(&42));
    });
}

#[test]
fn a_failed_initializer_hands_over_to_one_waiting_call() {
    check(|| async {
        let service = Service::new(3);
        let workers = (0..3).map(|_| {
            let service = service.clone();
            async move {
                let call = service
                    .connection
                    .get_or_try_init(|| service.clone().connect(true, 3));
                joining(call, &service.joined).await.copied()
            }
        });
        let mut results = testing::parallel(workers).await;
        results.sort();
        // Every call was waiting when the first initializer failed, so the
        // error reached only its own call and a waiting call took over.
        assert_eq!(results, [Ok(42), Ok(42), Err("down".into())]);
        let log = service.log();
        assert_eq!((log.started, log.finished, log.dropped), (2, 2, 0));
        assert_eq!(service.connection.get(), Some(&42));
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Left {
    // The call got the value.
    Stayed,
    // It left before its first poll.
    Early,
    // It left while it waited for another call's initializer.
    Waiting,
    // It left while its own initializer ran.
    Initializing,
}

type Worker = Pin<Box<dyn Future<Output = Option<Left>> + Send>>;

// Polls `call` once, then lets the calls waiting on `started` begin.
async fn first<F: Future>(call: F, started: &Cancellation) -> F::Output {
    let mut call = pin!(call);
    let mut first = true;
    poll_fn(|cx| {
        let polled = call.as_mut().poll(cx);
        if std::mem::take(&mut first) {
            started.cancel();
        }
        polled
    })
    .await
}

fn stayer(service: &Service, started: Option<&Cancellation>) -> Worker {
    let (service, started) = (service.clone(), started.cloned());
    Box::pin(async move {
        let call = service
            .connection
            .get_or_try_init(|| service.clone().connect(false, 0));
        let value = match started {
            Some(started) => first(call, &started).await,
            None => call.await,
        };
        assert_eq!(value, Ok(&42));
        None
    })
}

// A call that leaves when `leave` is requested, wherever it is.
fn leaver(service: &Service, leave: &Cancellation, started: Option<&Cancellation>) -> Worker {
    let (service, leave, started) = (service.clone(), leave.clone(), started.cloned());
    Box::pin(async move {
        let mut called = false;
        let mut pending = false;
        let result = {
            let call = service.connection.get_or_try_init_until(&leave, None, || {
                called = true;
                service.clone().connect(false, 0)
            });
            let mut call = pin!(call);
            let watched = poll_fn(|cx| {
                let poll = call.as_mut().poll(cx);
                pending |= poll.is_pending();
                poll
            });
            match started {
                Some(started) => first(watched, &started).await,
                None => watched.await,
            }
        };
        Some(match result {
            Ok(value) => {
                assert_eq!(value, &42);
                Left::Stayed
            }
            Err(ScopeError::Cancelled) if called => Left::Initializing,
            Err(ScopeError::Cancelled) if pending => Left::Waiting,
            Err(ScopeError::Cancelled) => Left::Early,
            Err(other) => panic!("{other:?}"),
        })
    })
}

// Runs a call that leaves on a request, beside two calls that stay. When
// `leaver_first`, the leaving call is polled before the others start, so it
// takes the claim; otherwise a staying call takes it. The request comes at
// any point after that first poll. Returns where the call left in each
// execution.
fn leave_anywhere(leaver_first: bool) -> Vec<Left> {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&outcomes);
    check(move || {
        let outcomes = Arc::clone(&observed);
        async move {
            let service = Service::new(0);
            let (leave, started) = (Cancellation::new(), Cancellation::new());
            let waiting = |worker: Worker| -> Worker {
                let started = started.clone();
                Box::pin(async move {
                    started.cancelled().await;
                    worker.await
                })
            };
            let workers = if leaver_first {
                [
                    leaver(&service, &leave, Some(&started)),
                    waiting(stayer(&service, None)),
                    waiting(stayer(&service, None)),
                ]
            } else {
                [
                    stayer(&service, Some(&started)),
                    waiting(leaver(&service, &leave, None)),
                    waiting(stayer(&service, None)),
                ]
            };
            let requester = waiting(Box::pin(async move {
                // Give the leaving call time to start waiting.
                yield_once().await;
                leave.cancel();
                None
            }));
            let results = testing::parallel(workers.into_iter().chain([requester])).await;
            let left = results.into_iter().flatten().next().unwrap();
            let log = service.log();
            assert_eq!(log.finished, 1, "{log:?}");
            assert_eq!(log.started, log.finished + log.dropped, "{log:?}");
            // Only the leaving call's initializer can be dropped, and a
            // staying call then ran its own.
            assert_eq!(
                log.dropped,
                u32::from(left == Left::Initializing),
                "{left:?}"
            );
            assert_eq!(service.connection.get(), Some(&42));
            outcomes.lock().unwrap().push(left);
        }
    });
    let outcomes = outcomes.lock().unwrap();
    outcomes.clone()
}

#[test]
fn a_call_that_leaves_while_its_initializer_runs_hands_over() {
    let outcomes = leave_anywhere(true);
    assert!(outcomes.contains(&Left::Initializing), "{outcomes:?}");
    assert!(!outcomes.contains(&Left::Waiting), "{outcomes:?}");
}

#[test]
fn a_call_that_leaves_while_it_waits_leaves_the_initializer_running() {
    let outcomes = leave_anywhere(false);
    assert!(outcomes.contains(&Left::Waiting), "{outcomes:?}");
    assert!(!outcomes.contains(&Left::Initializing), "{outcomes:?}");
}
