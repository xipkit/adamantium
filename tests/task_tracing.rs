#![cfg(all(feature = "tokio", feature = "tracing"))]
#![allow(
    clippy::disallowed_types,
    reason = "synchronous test subscriber state; locks never cross awaits"
)]

use std::{
    collections::BTreeMap,
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
    thread::ThreadId,
    time::Duration,
};

use adamantium::{
    Actor, AsyncActor, Backoff, Cancellation, Reply, Restarts, ScopeError, Tasks, time,
};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    instrument::WithSubscriber,
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Clone, Debug, Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[derive(Debug)]
struct Entered {
    fields: Fields,
    parent: Option<&'static str>,
    thread: ThreadId,
}

#[derive(Clone, Default)]
struct Capture {
    entered: Arc<Mutex<Vec<Entered>>>,
    events: Arc<Mutex<Vec<Fields>>>,
}

#[allow(clippy::unwrap_used, reason = "test subscriber invariants")]
impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        ctx.span(id).unwrap().extensions_mut().insert(fields);
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).unwrap();
        if span.name() == "adamantium.task" {
            self.entered.lock().unwrap().push(Entered {
                fields: span.extensions().get::<Fields>().unwrap().clone(),
                parent: span.parent().map(|parent| parent.name()),
                thread: std::thread::current().id(),
            });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let task = ctx.event_span(event).and_then(|span| {
            let extensions = span.extensions();
            extensions.get::<Fields>()?.0.get("task.name").cloned()
        });
        if let Some(task) = task {
            fields.0.insert("in task".to_owned(), task);
        }
        self.events.lock().unwrap().push(fields);
    }
}

async fn begin<F: Future + Send>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

async fn forever() -> Result<(), &'static str> {
    std::future::pending().await
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn deadlines_report_only_unfinished_named_tasks_in_body_and_join() {
    for pending_body in [false, true] {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        async {
            let deadline = time::now() + Duration::from_secs(10);
            let (ready, started) = Reply::pair();
            let mut scope = Box::pin(Tasks::run_until(
                Cancellation::new(),
                Some(deadline),
                async |tasks| {
                    let completed = tasks.spawn_named("completed", async { Ok(42) });
                    let failed =
                        tasks.spawn_named("handled error", async { Err::<(), _>("handled") });
                    tasks.spawn_named("stuck", forever());
                    let duplicate = tasks.spawn_named("duplicate", async { Ok(()) });
                    tasks.spawn_named("duplicate", forever());
                    tasks.spawn_named("duplicate", forever());
                    tasks.spawn(forever());
                    let (finished, finished_rx) = Reply::pair();
                    tasks.spawn_named("unawaited", async move {
                        finished.send(()).unwrap();
                        Ok(())
                    });
                    assert_eq!(tasks.value(completed).await, Ok(42));
                    assert_eq!(tasks.value(failed).await, Err("handled"));
                    tasks.value(duplicate).await.unwrap();
                    finished_rx.receive().await.unwrap();
                    ready.send(()).unwrap();
                    if pending_body {
                        forever().await?;
                    }
                    Ok(())
                },
            ));
            begin(scope.as_mut()).await;
            let mut started = Box::pin(started.receive());
            std::future::poll_fn(|cx| {
                assert!(scope.as_mut().poll(cx).is_pending());
                started.as_mut().poll(cx)
            })
            .await
            .unwrap();
            assert!(capture.events.lock().unwrap().is_empty());
            tokio::time::advance(Duration::from_secs(10)).await;
            assert_eq!(scope.await, Err(ScopeError::DeadlineExceeded));
        }
        .with_subscriber(subscriber)
        .await;

        let events = capture.events.lock().unwrap();
        let [report] = events.as_slice() else {
            panic!("expected one deadline report, got {events:?}");
        };
        assert_eq!(report.0.get("message").unwrap(), "scope deadline exceeded");
        assert_eq!(
            report.0.get("running_tasks").unwrap(),
            r#"["stuck", "duplicate", "duplicate"]"#,
        );
    }
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn deadline_reports_blocking_work_before_waiting_for_cleanup() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        let cancellation = Cancellation::new();
        let deadline = time::now() + Duration::from_secs(10);
        let (ready, started) = Reply::pair();
        #[expect(
            clippy::disallowed_methods,
            reason = "the synchronous test gate needs a bounded blocking wait"
        )]
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn_blocking_named("blocking", move || {
            ready.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(30)).unwrap();
            Ok(())
        });
        started.receive().await.unwrap();
        let mut join = Box::pin(tasks.join_until(cancellation.clone(), Some(deadline), Ok(())));
        begin(join.as_mut()).await;
        tokio::time::advance(Duration::from_secs(10)).await;
        begin(join.as_mut()).await;
        assert!(cancellation.is_cancelled());
        let events = capture.events.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.first().unwrap().0.get("running_tasks").unwrap(),
            r#"["blocking"]"#
        );
        release.send(()).unwrap();
        assert_eq!(join.await, Err(ScopeError::DeadlineExceeded));
        assert_eq!(capture.events.lock().unwrap().len(), 1);
    }
    .with_subscriber(subscriber)
    .await;
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn deadline_includes_named_work_that_has_not_been_polled() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = polled.clone();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn_named("queued", async move {
            flag.store(true, Ordering::SeqCst);
            forever().await
        });
        assert_eq!(
            tasks
                .join_until(Cancellation::new(), Some(time::now()), Ok(()))
                .await,
            Err(ScopeError::DeadlineExceeded),
        );
        assert!(!polled.load(Ordering::SeqCst));
    }
    .with_subscriber(subscriber)
    .await;
    let events = capture.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events.first().unwrap().0.get("running_tasks").unwrap(),
        r#"["queued"]"#
    );
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn panicked_tasks_are_excluded_and_their_panic_still_propagates() {
    use futures_util::FutureExt;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        let deadline = time::now() + Duration::from_secs(10);
        let (panicking, panicked) = Reply::pair();
        let mut scope = Box::pin(Tasks::run_until(
            Cancellation::new(),
            Some(deadline),
            async |tasks| {
                tasks.spawn_named("panicked", async move {
                    panicking.send(()).unwrap();
                    panic!("named child panic");
                    #[allow(unreachable_code)]
                    Ok(())
                });
                tasks.spawn_named("stuck", forever());
                forever().await
            },
        ));
        begin(scope.as_mut()).await;
        panicked.receive().await.unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        let panic = std::panic::AssertUnwindSafe(scope)
            .catch_unwind()
            .await
            .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"named child panic"));
    }
    .with_subscriber(subscriber)
    .await;
    let events = capture.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events.first().unwrap().0.get("running_tasks").unwrap(),
        r#"["stuck"]"#
    );
}

#[tokio::test]
async fn spans_follow_async_polls_and_blocking_closures_and_keep_the_parent() {
    check_spans(false).await;
    check_spans(true).await;
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(flavor = "multi_thread")]
async fn spans_follow_tasks_onto_worker_threads() {
    check_spans(false).await;
    check_spans(true).await;
}

#[allow(
    clippy::unwrap_used,
    reason = "shared assertions for both runtime flavors"
)]
async fn check_spans(use_spawner: bool) {
    use tracing::Instrument;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        let mut tasks = Tasks::<&str>::new();
        async {
            let future = async {
                assert_eq!(
                    tracing::Span::current().metadata().unwrap().name(),
                    "adamantium.task"
                );
                tokio::task::yield_now().await;
                assert_eq!(
                    tracing::Span::current().metadata().unwrap().name(),
                    "adamantium.task"
                );
                Ok(42)
            };
            let work = || {
                assert_eq!(
                    tracing::Span::current().metadata().unwrap().name(),
                    "adamantium.task"
                );
                Ok(std::thread::current().id())
            };
            let (value, blocking) = if use_spawner {
                let spawner = tasks.spawner();
                (
                    spawner.spawn_named("async", future).unwrap(),
                    spawner.spawn_blocking_named("blocking", work).unwrap(),
                )
            } else {
                (
                    tasks.spawn_named("async", future),
                    tasks.spawn_blocking_named("blocking", work),
                )
            };
            tasks.spawn(async { Ok(()) });
            assert_eq!(tasks.value(value).await, Ok(42));
            assert_eq!(
                tracing::Span::current().metadata().unwrap().name(),
                "parent"
            );
            let blocking_thread = tasks.value(blocking).await.unwrap();
            assert!(capture.entered.lock().unwrap().iter().any(|entered| {
                entered
                    .fields
                    .0
                    .get("task.name")
                    .is_some_and(|name| name == "blocking")
                    && entered.thread == blocking_thread
                    && entered.parent == Some("parent")
            }));
        }
        .instrument(tracing::info_span!("parent"))
        .await;
        tasks.join(Ok(())).await.unwrap();
        assert!(tracing::Span::current().is_none());
    }
    .with_subscriber(subscriber)
    .await;

    let entered = capture.entered.lock().unwrap();
    assert!(entered.iter().all(|span| span.parent == Some("parent")));
    assert!(entered.iter().any(|span| {
        span.fields
            .0
            .get("task.name")
            .is_some_and(|name| name == "async")
    }));
    assert!(capture.events.lock().unwrap().is_empty());
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn cancellation_and_unnamed_scopes_produce_no_diagnostics() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        let cancellation = Cancellation::new();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn_named("cancelled", forever());
        cancellation.cancel();
        assert_eq!(
            tasks
                .join_until(cancellation, Some(time::now()), Ok(()))
                .await,
            Err(ScopeError::Cancelled),
        );
        let named_span_count = capture.entered.lock().unwrap().len();
        let mut tasks = Tasks::<&str>::new();
        tasks.spawn(forever());
        tasks.spawn_blocking(|| Ok(()));
        assert_eq!(
            tasks
                .join_until(Cancellation::new(), Some(time::now()), Ok(()))
                .await,
            Err(ScopeError::DeadlineExceeded),
        );
        assert_eq!(capture.entered.lock().unwrap().len(), named_span_count);
    }
    .with_subscriber(subscriber)
    .await;
    assert!(capture.events.lock().unwrap().is_empty());
}

fn flaky(starts: &Actor<u32>) -> impl FnMut(Option<&'static str>) -> FlakyStart + Send + use<> {
    let starts = starts.clone();
    move |_| {
        let start = starts.with(|starts| {
            *starts += 1;
            *starts
        });
        Box::pin(async move { if start < 3 { Err("lost") } else { Ok(start) } })
    }
}

type FlakyStart = Pin<Box<dyn Future<Output = Result<u32, &'static str>> + Send>>;

#[tokio::test]
async fn a_named_supervised_task_keeps_one_span_and_reports_each_restart() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let starts = Actor::new(0);
    let result = async {
        Tasks::run(async |tasks| {
            let restarts = Restarts::new(5, Duration::from_secs(60));
            let worker = tasks.spawn_supervised_named("worker", restarts, flaky(&starts));
            tasks.value(worker).await
        })
        .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(result, Ok(3));

    let entered = capture.entered.lock().unwrap();
    assert!(!entered.is_empty());
    assert!(
        entered
            .iter()
            .all(|span| span.fields.0["task.name"] == "worker")
    );
    let events = capture.events.lock().unwrap();
    let restarts: Vec<_> = events
        .iter()
        .map(|event| (event.0["message"].as_str(), event.0["restart"].as_str()))
        .collect();
    assert_eq!(
        restarts,
        [
            ("supervised task restarting", "1"),
            ("supervised task restarting", "2"),
        ]
    );
    assert!(events.iter().all(|event| event.0["delay"] == "0ns"));
}

#[tokio::test]
async fn an_unnamed_supervised_task_reports_nothing() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let starts = Actor::new(0);
    let result = async {
        Tasks::run(async |tasks| {
            let restarts = Restarts::new(5, Duration::from_secs(60));
            let worker = tasks.spawn_supervised(restarts, flaky(&starts));
            tasks.value(worker).await
        })
        .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(result, Ok(3));
    assert!(capture.entered.lock().unwrap().is_empty());
    assert!(capture.events.lock().unwrap().is_empty());
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn a_deadline_reports_a_supervised_task_that_is_waiting_to_restart() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let result = async {
        let deadline = time::now() + Duration::from_secs(10);
        Tasks::run_until(Cancellation::new(), Some(deadline), async |tasks| {
            let restarts = Restarts::new(5, Duration::from_secs(60))
                .backoff(Backoff::constant(Duration::from_secs(3600)));
            tasks.spawn_supervised_named("worker", restarts, |_| async { Err::<(), _>("lost") });
            forever().await
        })
        .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(result, Err(ScopeError::DeadlineExceeded));

    let events = capture.events.lock().unwrap();
    let messages: Vec<_> = events
        .iter()
        .map(|event| event.0["message"].as_str())
        .collect();
    assert_eq!(
        messages,
        ["supervised task restarting", "scope deadline exceeded"]
    );
    assert_eq!(events[0].0["delay"], "3600s");
    assert_eq!(events[1].0["running_tasks"], "[\"worker\"]");
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn a_named_handled_task_runs_its_handler_in_its_span_and_is_reported_until_it_ends() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let deadline = time::now() + Duration::from_secs(10);
    let result = async {
        Tasks::<&str>::run_until(Cancellation::new(), Some(deadline), async |tasks| {
            let (handled, handled_rx) = Reply::pair();
            let spawner = tasks.spawner();
            spawner
                .spawn_handled_named("failed", async { Err("reset") }, move |error| {
                    tracing::warn!(error, "connection failed");
                    handled.send(()).unwrap();
                })
                .unwrap();
            spawner
                .spawn_handled_named("stuck", forever(), |_| {})
                .unwrap();
            handled_rx.receive().await.unwrap();
            forever().await
        })
        .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(result, Err(ScopeError::DeadlineExceeded));

    let entered = capture.entered.lock().unwrap();
    let names: Vec<_> = entered
        .iter()
        .map(|span| span.fields.0["task.name"].as_str())
        .collect();
    assert!(
        names.contains(&"failed") && names.contains(&"stuck"),
        "{names:?}"
    );
    let events = capture.events.lock().unwrap();
    let messages: Vec<_> = events
        .iter()
        .map(|event| event.0["message"].as_str())
        .collect();
    assert_eq!(messages, ["connection failed", "scope deadline exceeded"]);
    assert_eq!(events[0].0["in task"], "failed");
    assert_eq!(events[1].0["running_tasks"], r#"["stuck"]"#);
}

#[tokio::test]
async fn a_named_async_actor_runs_its_handler_in_a_child_task_span() {
    use tracing::Instrument;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    async {
        Tasks::<&str>::run(async |tasks| {
            async {
                let actor = AsyncActor::new_named(
                    tasks,
                    "db-conn",
                    (),
                    2,
                    |(), (label, reply): (&'static str, Reply<&'static str>)| {
                        Box::pin(async move {
                            tracing::info!(label, "handling actor message");
                            tokio::task::yield_now().await;
                            assert_eq!(
                                tracing::Span::current().metadata().unwrap().name(),
                                "adamantium.task"
                            );
                            reply.send(label).unwrap();
                            Ok(())
                        })
                    },
                );
                assert_eq!(actor.call(|reply| ("ping", reply)).await, Some("ping"));
                actor.close();
                Ok(())
            }
            .instrument(tracing::info_span!("request"))
            .await
        })
        .await
        .unwrap();
    }
    .with_subscriber(subscriber)
    .await;

    let entered = capture.entered.lock().unwrap();
    assert!(!entered.is_empty());
    assert!(entered.iter().all(|span| {
        span.fields.0.get("task.name").map(String::as_str) == Some("db-conn")
            && span.parent == Some("request")
    }));
    let events = capture.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0["message"], "handling actor message");
    assert_eq!(events[0].0["in task"], "db-conn");
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test(start_paused = true)]
async fn a_named_async_actor_is_reported_at_deadline_until_closed() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let deadline = time::now() + Duration::from_secs(10);
    let result = async {
        Tasks::<&str>::run_until(Cancellation::new(), Some(deadline), async |tasks| {
            let done = AsyncActor::new_named(tasks, "finished-actor", (), 1, |(), r: Reply<()>| {
                Box::pin(async move {
                    r.send(()).unwrap();
                    Ok(())
                })
            });
            let stuck = AsyncActor::new_named(tasks, "stuck-actor", (), 1, |(), (): ()| {
                Box::pin(async move { forever().await })
            });
            done.call(|r| r).await.unwrap();
            done.close();
            stuck.send(()).await.unwrap();
            forever().await
        })
        .await
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(result, Err(ScopeError::DeadlineExceeded));

    let events = capture.events.lock().unwrap();
    let [report] = events.as_slice() else {
        panic!("expected one deadline report, got {events:?}");
    };
    assert_eq!(report.0["message"], "scope deadline exceeded");
    assert_eq!(report.0["running_tasks"], r#"["stuck-actor"]"#);
}
