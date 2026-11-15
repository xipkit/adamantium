#![cfg(feature = "tokio")]

use adamantium::{Actor, Restarts, Tasks};
use std::time::Duration;

const MINUTE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn a_failing_task_restarts_until_it_succeeds() {
    let seen = Actor::new(Vec::new());
    let log = seen.clone();
    let result = Tasks::run(async |tasks| {
        let worker = tasks.spawn_supervised(Restarts::new(5, MINUTE), move |error| {
            let starts = log.with(|seen| {
                seen.push(error);
                seen.len()
            });
            async move {
                if starts < 3 {
                    Err(starts)
                } else {
                    Ok("served")
                }
            }
        });
        tasks.value(worker).await
    })
    .await;
    assert_eq!(result, Ok("served"));
    assert_eq!(seen.snapshot(), [None, Some(1), Some(2)]);
}

#[tokio::test]
async fn a_task_out_of_restarts_fails_the_scope_with_its_last_error() {
    let starts = Actor::new(0);
    let count = starts.clone();
    let result = Tasks::run(async |tasks| {
        let worker = tasks.spawn_supervised(Restarts::new(2, MINUTE), move |_| {
            let start = count.with(|starts| {
                *starts += 1;
                *starts
            });
            async move { Err::<(), _>(start) }
        });
        tasks.value(worker).await
    })
    .await;
    assert_eq!(result, Err(3));
    assert_eq!(starts.snapshot(), 3);
}

#[tokio::test]
async fn a_limit_of_zero_never_restarts() {
    let starts = Actor::new(0);
    let count = starts.clone();
    let result = Tasks::run(async |tasks| {
        let worker = tasks.spawn_supervised(Restarts::new(0, MINUTE), move |_| {
            count.with(|starts| *starts += 1);
            async { Err::<(), _>("failed") }
        });
        tasks.value(worker).await
    })
    .await;
    assert_eq!(result, Err("failed"));
    assert_eq!(starts.snapshot(), 1);
}

#[tokio::test]
async fn an_error_nobody_claims_is_returned_when_the_scope_joins() {
    let result = Tasks::run(async |tasks| {
        drop(tasks.spawn_supervised(Restarts::new(1, MINUTE), |_| async {
            Err::<(), _>("failed")
        }));
        Ok(())
    })
    .await;
    assert_eq!(result, Err("failed"));
}

#[tokio::test]
#[should_panic(expected = "not an error")]
async fn a_panic_is_not_restarted() {
    let result = Tasks::<&str>::run(async |tasks| {
        let worker = tasks.spawn_supervised(Restarts::new(5, MINUTE), |error| async move {
            assert!(error.is_none(), "the task restarted");
            if error.is_none() {
                panic!("not an error");
            }
            Ok(())
        });
        tasks.value(worker).await
    })
    .await;
    assert_eq!(result, Ok(()), "the panic was not resumed");
}

#[cfg(not(target_os = "wasi"))]
mod paused {
    use super::*;
    use adamantium::{Backoff, time};

    #[tokio::test(start_paused = true)]
    async fn restarts_older_than_the_window_do_not_count() {
        let starts = Actor::new(0);
        let count = starts.clone();
        let restarts = Restarts::new(1, Duration::from_secs(10));
        let result = Tasks::run(async |tasks| {
            let worker = tasks.spawn_supervised(restarts, move |_| {
                let start = count.with(|starts| {
                    *starts += 1;
                    *starts
                });
                async move {
                    // The second start stays healthy for longer than the
                    // window, so the first restart no longer counts when it
                    // fails, and it restarts too. The third start fails
                    // inside that restart's window.
                    if start == 2 {
                        time::sleep(Duration::from_secs(11)).await;
                    }
                    Err::<(), _>(start)
                }
            });
            tasks.value(worker).await
        })
        .await;
        assert_eq!(result, Err(3));
    }

    #[tokio::test(start_paused = true)]
    async fn the_backoff_waits_longer_for_each_restart_in_the_window() {
        let began = time::now();
        let seen = Actor::new(Vec::new());
        let log = seen.clone();
        let backoff = Backoff::exponential(Duration::from_secs(1), Duration::from_secs(4));
        let restarts = Restarts::new(4, MINUTE).backoff(backoff);
        let result = Tasks::run(async |tasks| {
            let worker = tasks.spawn_supervised(restarts, move |_| {
                log.with(|seen| seen.push((time::now() - began).as_secs()));
                async { Err::<(), _>("failed") }
            });
            tasks.value(worker).await
        })
        .await;
        assert_eq!(result, Err("failed"));
        assert_eq!(seen.snapshot(), [0, 1, 3, 7, 11]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_backoff_attempts_are_ignored() {
        let starts = Actor::new(0);
        let count = starts.clone();
        let backoff = Backoff::constant(Duration::from_secs(1)).attempts(1);
        let restarts = Restarts::new(3, MINUTE).backoff(backoff);
        let result = Tasks::run(async |tasks| {
            let worker = tasks.spawn_supervised(restarts, move |_| {
                count.with(|starts| *starts += 1);
                async { Err::<(), _>("failed") }
            });
            tasks.value(worker).await
        })
        .await;
        assert_eq!(result, Err("failed"));
        assert_eq!(starts.snapshot(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_scope_does_not_wait_for_a_restart_that_is_backing_off() {
        let began = time::now();
        let backoff = Backoff::constant(Duration::from_secs(3600));
        let restarts = Restarts::new(5, MINUTE).backoff(backoff);
        let result = Tasks::run(async |tasks| {
            drop(tasks.spawn_supervised(restarts, |_| async { Err::<(), _>("task") }));
            time::sleep(Duration::from_secs(1)).await;
            Err::<(), _>("body")
        })
        .await;
        assert_eq!(result, Err("body"));
        assert_eq!((time::now() - began).as_secs(), 1);
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn every_schedule_restarts_the_task_and_keeps_its_value() {
    use adamantium::{
        Backoff,
        testing::{self, Config},
    };

    let config = Config {
        seed: 183,
        iterations: 200,
        max_steps: 20_000,
    };
    let checked = testing::check(config, || async {
        let starts = Actor::new(0);
        let count = starts.clone();
        let restarts = Restarts::new(3, MINUTE).backoff(Backoff::constant(Duration::from_secs(1)));
        let result = Tasks::run(async |tasks| {
            let worker = tasks.spawn_supervised(restarts, move |_| {
                let start = count.with(|starts| {
                    *starts += 1;
                    *starts
                });
                async move { if start < 3 { Err(start) } else { Ok(start) } }
            });
            let other = tasks.spawn(async { Ok(7) });
            Ok((tasks.value(worker).await?, tasks.value(other).await?))
        })
        .await;
        assert_eq!(result, Ok((3, 7)));
    });
    assert!(checked.is_ok(), "{checked:?}");
}
