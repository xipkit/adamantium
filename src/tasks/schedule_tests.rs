use super::*;
use crate::testing::{self, Config};

#[test]
fn oldest_unclaimed_error_and_body_precedence_survive_replay() {
    async fn scenario() {
        let mut tasks = Tasks::<&str>::new();
        let first = tasks.spawn(async { Err::<(), _>("first") });
        while tasks.spawner.ledger.lock().recorded.len() != 1 {
            shuttle::future::yield_now().await;
        }
        let second = tasks.spawn(async { Err::<(), _>("second") });
        while tasks.spawner.ledger.lock().recorded.len() != 2 {
            shuttle::future::yield_now().await;
        }
        assert_eq!(tasks.value(first).await, Err("first"));
        let third = tasks.spawn(async { Err::<(), _>("third") });
        while tasks.spawner.ledger.lock().recorded.len() != 2 {
            shuttle::future::yield_now().await;
        }
        // Retained result handles cannot hide errors from join.
        assert_eq!(tasks.join(Ok(())).await, Err("second"));
        drop((second, third));

        let mut tasks = Tasks::<&str>::new();
        let child = tasks.spawn(async { Err::<(), _>("child") });
        while !tasks.failed() {
            shuttle::future::yield_now().await;
        }
        assert_eq!(tasks.join(Err::<(), _>("body")).await, Err("body"));
        drop(child);
    }
    let config = Config {
        seed: 160,
        iterations: 500,
        max_steps: 20_000,
        ..Config::default()
    };
    let replay = testing::check(config, scenario).unwrap();
    assert_eq!(testing::check(config, scenario).unwrap(), replay);
    assert_eq!(testing::replay(&replay, scenario).unwrap(), replay);
}
