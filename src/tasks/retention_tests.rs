use super::*;
use crate::Reply;
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, pin::pin};

async fn yield_now() {
    #[cfg(feature = "deterministic")]
    if World::current().is_model() {
        shuttle::future::yield_now().await;
        return;
    }
    tokio::task::yield_now().await;
}

struct Dropped(crate::Actor<bool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.with(|dropped| *dropped = true);
    }
}

enum Reap {
    Spawn,
    SpawnBlocking,
}

#[allow(clippy::disallowed_methods, reason = "test watchdog")]
fn watchdog<F: Future>(future: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout(Duration::from_secs(30), future)
}

#[cfg(feature = "deterministic")]
fn check_and_replay<F, Fut>(seed: u64, iterations: usize, max_steps: usize, scenario: F)
where
    F: Fn() -> Fut + Copy + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let config = crate::testing::Config {
        seed,
        iterations,
        max_steps,
        ..crate::testing::Config::default()
    };
    let replay = crate::testing::check(config, scenario).unwrap();
    assert_eq!(crate::testing::replay(&replay, scenario).unwrap(), replay);
}

#[tokio::test]
async fn aborted_work_stays_unfinished_until_it_is_dropped() {
    let mut tasks = Tasks::<()>::new();
    tasks.spawn(std::future::pending::<Result<(), ()>>());
    tasks.spawn(std::future::pending::<Result<(), ()>>());
    assert_eq!(tasks.len(), 2);
    tasks.abort();
    assert_eq!(tasks.len(), 2);
    tasks.drain().await;
    assert!(tasks.is_empty());
}

async fn panic_scenario() {
    for blocking in [false, true] {
        for through_value in [false, true] {
            for reaper in [Reap::Spawn, Reap::SpawnBlocking] {
                let mut tasks = Tasks::<&str>::new();
                let failed = if blocking {
                    tasks.spawn_blocking::<(), _>(|| panic!("reaped panic"))
                } else {
                    tasks.spawn::<(), _>(async { panic!("reaped panic") })
                };
                while !failed.rx.is_closed() {
                    yield_now().await;
                }
                while tasks.spawner.ledger.lock().panic.is_none() {
                    match reaper {
                        Reap::Spawn => drop(tasks.spawn(async { Ok(()) })),
                        Reap::SpawnBlocking => drop(tasks.spawn_blocking(|| Ok(()))),
                    }
                    yield_now().await;
                }

                // Work added after reaping must also be drained before the
                // saved panic is resumed.
                let dropped = crate::Actor::new(false);
                let guard = Dropped(dropped.clone());
                tasks.spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    Ok(())
                });
                let panic = AssertUnwindSafe(async move {
                    if through_value {
                        tasks.value(failed).await
                    } else {
                        tasks.join(Err::<(), _>("body error")).await
                    }
                })
                .catch_unwind()
                .await
                .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"reaped panic"));
                assert!(dropped.snapshot());
            }
        }
    }
}

#[cfg_attr(target_os = "wasi", ignore = "WASI aborts on panic")]
#[tokio::test]
async fn reaped_panics_reach_value_and_join_after_cleanup() {
    watchdog(panic_scenario()).await.unwrap();
}

#[cfg(feature = "deterministic")]
#[test]
fn reaped_panics_survive_deterministic_replay() {
    check_and_replay(187, 100, 30_000, panic_scenario);
}

#[tokio::test]
async fn spawning_keeps_retained_entries_bounded_across_100_000_tasks() {
    Tasks::<()>::run(async |tasks| {
        for _ in 0..1_000 {
            let handles: Vec<_> = (0..100).map(|_| tasks.spawn(async { Ok(()) })).collect();
            assert!(
                tasks.spawner.set.lock("JoinSet::len").len() <= 100,
                "completed entries accumulated"
            );
            for handle in handles {
                tasks.value(handle).await?;
            }
        }
        assert_eq!(tasks.len(), 0);
        for _ in 0..1_000 {
            for _ in 0..100 {
                tasks.spawn_handled(async { Ok::<_, ()>(()) }, |()| {});
            }
            assert!(
                tasks.spawner.set.lock("JoinSet::len").len() <= 100,
                "completed handled entries accumulated"
            );
            while !tasks.is_empty() {
                yield_now().await;
            }
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn handled_spawner_reaps_completed_entries_before_join() {
    let tasks = Tasks::<()>::new();
    let spawner = tasks.spawner();
    watchdog(async {
        for _ in 0..100 {
            spawner.spawn_handled(async { Err(()) }, |()| {}).unwrap();
            while !tasks.is_empty() {
                yield_now().await;
            }
            // One task entry and at most one coalesced completion.
            assert!(tasks.spawner.set.lock("JoinSet::len").len() <= 2);
        }
    })
    .await
    .unwrap();
    assert_eq!(tasks.join(Ok(())).await, Ok(()));
}

#[cfg(not(target_os = "wasi"))]
#[tokio::test]
async fn spawner_reaping_leaves_panics_for_scope_shutdown() {
    let tasks = Tasks::<()>::new();
    let spawner = tasks.spawner();
    spawner
        .spawn_handled(async { panic!("handled task panic") }, |(): ()| {})
        .unwrap();
    watchdog(async {
        while !tasks.is_empty() {
            yield_now().await;
        }
        spawner
            .spawn_handled(std::future::pending(), |(): ()| {})
            .unwrap();
        // The second submission reaps the first task. Joining must
        // still see its panic and abort the pending sibling.
        let panic = AssertUnwindSafe(tasks.join(Ok(())))
            .catch_unwind()
            .await
            .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"handled task panic"));
    })
    .await
    .unwrap();
}

async fn reaped_error_during_join() {
    let mut tasks = Tasks::<&str>::new();
    let spawner = tasks.spawner();
    let set = Arc::clone(&tasks.spawner.set);
    let (release, released) = Reply::pair();
    let failed = tasks.spawn(async move {
        released.receive().await.unwrap();
        Err::<(), _>("child failed")
    });

    let mut join = pin!(tasks.join(Ok(())));
    poll_fn(|cx| {
        assert!(join.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    release.send(()).unwrap();
    while set.lock("JoinSet::unfinished").unfinished() != 0 {
        yield_now().await;
    }

    let dropped = crate::Actor::new(false);
    let guard = Dropped(dropped.clone());
    spawner
        .spawn_handled(
            async move {
                let _guard = guard;
                std::future::pending::<Result<(), ()>>().await
            },
            |()| {},
        )
        .unwrap();
    assert_eq!(join.await, Err("child failed"));
    assert!(dropped.snapshot());
    drop(failed);
}

#[tokio::test]
async fn join_observes_a_child_error_even_when_a_spawner_reaps_its_completion() {
    watchdog(reaped_error_during_join()).await.unwrap();
}

#[cfg(feature = "deterministic")]
#[test]
fn join_observes_a_reaped_completion_under_replay() {
    check_and_replay(224, 100, 10_000, reaped_error_during_join);
}

// Spawns connections that finish at once, then waits without spawning
// again: only the running scope's own reaping can let go of them.
async fn idle_server_scenario() {
    Tasks::<()>::run(async |tasks| {
        let served = crate::Actor::new(0);
        for _ in 0..100 {
            let served = served.clone();
            tasks.spawn_handled(
                async move {
                    served.with(|served| *served += 1);
                    Ok::<_, ()>(())
                },
                |()| panic!("the connection succeeded"),
            );
        }
        while served.snapshot() < 100 || !tasks.spawner.set.lock("JoinSet::is_empty").is_empty() {
            crate::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_running_scope_reaps_tasks_without_another_spawn() {
    watchdog(idle_server_scenario()).await.unwrap();
}

#[cfg(feature = "deterministic")]
#[test]
fn a_running_scope_reaps_tasks_without_another_spawn_under_replay() {
    check_and_replay(195, 50, 100_000, idle_server_scenario);
}

// The scope's loop and the next spawn race to reap the same panic.
// Whichever finds it, the body stops and the sibling is drained.
#[cfg(feature = "deterministic")]
async fn panic_reaped_by_either_scenario() {
    let dropped = crate::Actor::new(false);
    let guard = Dropped(dropped.clone());
    let scope = Tasks::<()>::run(async move |tasks| {
        tasks.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Ok(())
        });
        let failed = tasks.spawn::<(), _>(async { panic!("reaped panic") });
        while !failed.rx.is_closed() {
            yield_now().await;
        }
        drop(tasks.spawn(async { Ok(()) }));
        std::future::pending::<Result<(), ()>>().await
    });
    let panic = AssertUnwindSafe(scope).catch_unwind().await.unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"reaped panic"));
    assert!(dropped.snapshot());
}

#[cfg(feature = "deterministic")]
#[test]
fn a_panic_reaped_by_a_spawn_still_stops_the_body() {
    check_and_replay(195, 200, 30_000, panic_reaped_by_either_scenario);
}

#[cfg(feature = "deterministic")]
#[test]
fn finished_tasks_are_reaped_across_many_modeled_batches() {
    async fn scenario() {
        Tasks::<()>::run(async |tasks| {
            for _ in 0..100 {
                let retained = tasks.spawn(async { Ok(42) });
                let blocking = tasks.spawn_blocking(|| Ok(7));
                while !tasks.is_empty() {
                    yield_now().await;
                }
                let trigger = tasks.spawn(async { Ok(()) });
                assert_eq!(tasks.spawner.set.lock("JoinSet::len").len(), 1);
                // Reaping only releases the scope's bookkeeping. Handles
                // continue to own their results until value is requested.
                assert_eq!(tasks.value(retained).await, Ok(42));
                assert_eq!(tasks.value(blocking).await, Ok(7));
                assert_eq!(tasks.value(trigger).await, Ok(()));
            }
            Ok(())
        })
        .await
        .unwrap();
    }
    check_and_replay(187, 20, 100_000, scenario);
}
