//! `SingleFlight` under the deterministic harness, with a model of Clu's
//! store sync as the fixture.
//!
//! Clu commits control-plane writes to a local database and acknowledges a
//! write only once a push to the remote database covers it. Concurrent
//! acknowledgments share a push. A push covers the commits made before it
//! started, so an acknowledgment that joined an older push calls again; the
//! push it joins then started after the first one ended.

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
    Actor, Channel, SingleFlight, race,
    testing::{self, Config},
};

type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

fn config() -> Config {
    Config {
        seed: 200,
        iterations: 300,
        max_steps: 20_000,
        ..Config::default()
    }
}

/// Explores the schedules, then checks that they repeat and replay.
fn check<F, Fut>(factory: F)
where
    F: Fn() -> Fut + Send + Sync + 'static + Clone,
    Fut: Future<Output = ()>,
{
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Remote {
    /// The newest commit the remote database has.
    pushed: u64,
    /// Pushes started, finished or not.
    started: u32,
    /// Pushes dropped before they finished.
    cancelled: u32,
    /// Whether a dropped push is still cleaning up.
    cleaning: bool,
    /// Pushes that started while a dropped one was still cleaning up.
    started_during_cleanup: u32,
}

/// One control-plane database and its connection to the remote copy.
#[derive(Clone)]
struct Store {
    /// The newest local commit.
    commits: Actor<u64>,
    remote: Actor<Remote>,
    /// Each push waits for one answer from the network.
    network: Channel<Result<(), String>>,
    /// Told when a dropped push begins to clean up.
    cleanups: Channel<()>,
    sync: SingleFlight<&'static str, Result<u64, String>>,
}

impl Store {
    fn new() -> Self {
        Self {
            commits: Actor::new(0),
            remote: Actor::default(),
            network: Channel::bounded(8),
            cleanups: Channel::bounded(8),
            sync: SingleFlight::new(),
        }
    }

    fn commit(&self) -> u64 {
        self.commits.with(|commits| {
            *commits += 1;
            *commits
        })
    }

    /// Waits until a push covers `commit`.
    async fn acknowledge(&self, commit: u64) -> Result<(), String> {
        loop {
            let covered = self.sync.run("control", || self.clone().push()).await?;
            if covered >= commit {
                return Ok(());
            }
        }
    }

    /// Pushes every commit made before it starts, and returns the newest.
    async fn push(self) -> Result<u64, String> {
        let target = self.commits.snapshot();
        self.remote.with(|remote| {
            remote.started += 1;
            if remote.cleaning {
                remote.started_during_cleanup += 1;
            }
        });
        let mut cleanup = Cleanup {
            remote: self.remote.clone(),
            cleanups: self.cleanups.clone(),
            finished: false,
        };
        let answer = self.network.receive().await;
        cleanup.finished = true;
        answer.unwrap_or(Err("network closed".into()))?;
        self.remote
            .with(|remote| remote.pushed = remote.pushed.max(target));
        Ok(target)
    }

    fn remote(&self) -> Remote {
        self.remote.snapshot()
    }

    fn answer(&self, answer: Result<(), String>) {
        self.network.try_send(answer).unwrap();
    }
}

/// How many steps a dropped push takes to close its connection.
const CLOSING_STEPS: usize = 8;

/// Records a push dropped before it finished, then closes its connection
/// over several steps, between any two of which another task can run.
struct Cleanup {
    remote: Actor<Remote>,
    cleanups: Channel<()>,
    finished: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.remote.with(|remote| {
            remote.cancelled += 1;
            remote.cleaning = true;
        });
        self.cleanups.try_send(()).unwrap();
        for _ in 0..CLOSING_STEPS {
            self.remote.read(|_| ());
        }
        self.remote.with(|remote| remote.cleaning = false);
    }
}

/// Runs `call`, sending on `joined` once it has been polled, which is when
/// it starts or joins a push.
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

/// Runs `call` until it finishes or `leave` is sent, which drops it.
async fn leaving<F: Future>(call: F, leave: &Channel<()>) -> Option<F::Output> {
    race((async { Some(call.await) }, async {
        leave.receive().await;
        None
    }))
    .await
}

/// Counts the executions in which `seen` held, across every exploration.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<usize>>);

impl Seen {
    fn record(&self, seen: bool) {
        if seen {
            *self.0.lock().unwrap() += 1;
        }
    }

    fn count(&self) -> usize {
        *self.0.lock().unwrap()
    }
}

#[test]
fn concurrent_acknowledgments_share_one_push() {
    check(|| async {
        let store = Store::new();
        let commits = [store.commit(), store.commit(), store.commit()];
        let joined = Channel::bounded(3);
        let workers = commits.map(|commit| {
            let (store, joined) = (store.clone(), joined.clone());
            Box::pin(async move { joining(store.acknowledge(commit), &joined).await }) as _
        });
        let network: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin({
            let store = store.clone();
            async move {
                for _ in 0..3 {
                    joined.receive().await;
                }
                store.answer(Ok(()));
                Ok(())
            }
        });
        let results = testing::parallel(workers.into_iter().chain([network])).await;
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let remote = store.remote();
        assert_eq!((remote.started, remote.pushed), (1, 3));
        assert!(store.sync.is_empty());
    });
}

#[test]
fn every_acknowledged_commit_is_on_the_remote_however_writes_interleave() {
    let shared = Seen::default();
    let retried = Seen::default();
    let observed = (shared.clone(), retried.clone());
    check(move || {
        let (shared, retried) = observed.clone();
        async move {
            let store = Store::new();
            let calls = Actor::new(0_u32);
            for _ in 0..3 {
                store.answer(Ok(()));
            }
            let workers = (0..3).map(|_| {
                let (store, calls) = (store.clone(), calls.clone());
                async move {
                    let commit = store.commit();
                    loop {
                        calls.with(|calls| *calls += 1);
                        let covered = store.sync.run("control", || store.clone().push()).await;
                        if covered.unwrap() >= commit {
                            break;
                        }
                    }
                    assert!(store.remote().pushed >= commit);
                }
            });
            testing::parallel(workers).await;
            let remote = store.remote();
            assert!(remote.started <= 3, "each writer starts one push at most");
            assert!(store.sync.is_empty());
            shared.record(remote.started < 3);
            retried.record(calls.snapshot() > 3);
        }
    });
    assert!(shared.count() > 0, "no schedule shared a push");
    assert!(retried.count() > 0, "no schedule joined a push too old");
}

#[test]
fn dropping_the_first_caller_lets_the_others_finish() {
    let left_first = Seen::default();
    let observed = left_first.clone();
    check(move || {
        let left_first = observed.clone();
        async move {
            let store = Store::new();
            let commit = store.commit();
            let (started, joined, leave) = (
                Channel::bounded(1),
                Channel::bounded(1),
                Channel::bounded(1),
            );
            let first: Worker = Box::pin({
                let (store, started, leave) = (store.clone(), started.clone(), leave.clone());
                async move {
                    let left = leaving(joining(store.acknowledge(commit), &started), &leave).await;
                    left_first.record(left.is_none());
                }
            });
            let second: Worker = Box::pin({
                let (store, joined) = (store.clone(), joined.clone());
                async move {
                    started.receive().await;
                    joining(store.acknowledge(commit), &joined).await.unwrap();
                }
            });
            let driver: Worker = Box::pin({
                let store = store.clone();
                async move {
                    joined.receive().await;
                    // The first caller leaves while the push may finish.
                    leave.try_send(()).unwrap();
                    store.answer(Ok(()));
                }
            });
            testing::parallel([first, second, driver]).await;
            let remote = store.remote();
            assert_eq!((remote.started, remote.cancelled, remote.pushed), (1, 0, 1));
            assert!(store.sync.is_empty());
        }
    });
    assert!(left_first.count() > 0, "the first caller never left early");
}

#[test]
fn the_last_caller_leaving_cancels_the_push_and_the_next_one_retries() {
    check(|| async {
        let store = Store::new();
        let commit = store.commit();
        let (started, leave) = (Channel::bounded(1), Channel::bounded(1));
        let left = race((
            leaving(joining(store.acknowledge(commit), &started), &leave),
            async {
                started.receive().await;
                leave.try_send(()).unwrap();
                std::future::pending().await
            },
        ))
        .await;
        assert_eq!(left, None);
        let remote = store.remote();
        assert_eq!((remote.started, remote.cancelled, remote.pushed), (1, 1, 0));
        assert!(store.sync.is_empty(), "the idle entry was removed");

        store.answer(Ok(()));
        store.acknowledge(commit).await.unwrap();
        let remote = store.remote();
        assert_eq!((remote.started, remote.pushed), (2, 1));
        assert!(store.sync.is_empty());
    });
}

#[test]
fn a_call_arriving_during_cleanup_starts_a_new_push() {
    let during = Seen::default();
    let observed = during.clone();
    check(move || {
        let during = observed.clone();
        async move {
            let store = Store::new();
            let commit = store.commit();
            let (started, leave) = (Channel::bounded(1), Channel::bounded(1));
            store.answer(Ok(()));
            // Hold the answer back until the first push is gone.
            let answer = store.network.try_receive().unwrap();
            let first: Worker = Box::pin({
                let (store, started, leave) = (store.clone(), started.clone(), leave.clone());
                async move {
                    let left = leaving(joining(store.acknowledge(commit), &started), &leave).await;
                    assert_eq!(left, None);
                }
            });
            let second: Worker = Box::pin({
                let store = store.clone();
                async move {
                    started.receive().await;
                    leave.try_send(()).unwrap();
                    // Arrive once the first push has been dropped, while
                    // its cleanup may still be running.
                    store.cleanups.receive().await;
                    let call = store.acknowledge(commit);
                    let mut call = pin!(call);
                    let polled = poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await;
                    assert!(polled.is_pending(), "a push waits for the network");
                    store.network.try_send(answer.clone()).unwrap();
                    call.await.unwrap();
                }
            });
            testing::parallel([first, second]).await;
            let remote = store.remote();
            assert_eq!((remote.started, remote.cancelled, remote.pushed), (2, 1, 1));
            assert!(!remote.cleaning);
            assert!(store.sync.is_empty());
            during.record(remote.started_during_cleanup > 0);
        }
    });
    assert!(during.count() > 0, "no call arrived during cleanup");
}

#[test]
fn a_failed_push_reaches_every_caller_and_the_next_acknowledgment_retries() {
    check(|| async {
        let store = Store::new();
        let commits = [store.commit(), store.commit()];
        let joined = Channel::bounded(2);
        let workers = commits.map(|commit| {
            let (store, joined) = (store.clone(), joined.clone());
            Box::pin(async move { joining(store.acknowledge(commit), &joined).await }) as _
        });
        let network: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin({
            let store = store.clone();
            async move {
                for _ in 0..2 {
                    joined.receive().await;
                }
                store.answer(Err("remote unavailable".into()));
                Ok(())
            }
        });
        let results = testing::parallel(workers.into_iter().chain([network])).await;
        let failed = Err("remote unavailable".to_string());
        assert_eq!(results, [failed.clone(), failed, Ok(())]);
        let remote = store.remote();
        assert_eq!((remote.started, remote.pushed), (1, 0));
        assert!(store.sync.is_empty(), "the failure is not kept");

        store.answer(Ok(()));
        store.acknowledge(commits[1]).await.unwrap();
        let remote = store.remote();
        assert_eq!((remote.started, remote.pushed), (2, 2));
        assert!(store.sync.is_empty());
    });
}
