# Deterministic Actor tests

The `deterministic` feature lets a test explore concurrent interleavings and replay saved schedules. Seeded checks repeat the same scheduling choices for the same scenario and inputs.

Enable `deterministic` in a test dependency. The backend is selected for each execution. In the same build, ordinary application code and Tokio tests continue to use the production primitives.

```rust
use adamantium::{Actor, testing::{self, Config}};

#[test]
fn concurrent_updates() {
    testing::check(Config { seed: 143, ..Config::default() }, || async {
        let count = Actor::new(0);
        let workers = (0..2).map(|_| {
            let count = count.clone();
            async move { count.with(|n| *n += 1); }
        }).collect::<Vec<_>>();
        testing::parallel(workers).await;
        assert_eq!(count.into_inner().ok(), Some(2));
    }).unwrap();
}
```

## How a test runs

- The factory closure creates new state for every execution.
- The harness also schedules synchronous `Actor` calls and ownership operations. A `read` followed by a separate `with` can thus show a lost update, even with no `await` between them.
- `testing::parallel` provides independent test workers.
- A panic, deadlock, or exhaustion of the per-execution step limit stops exploration and returns a failure.

## Choose a scheduler

Pass `Config` directly for seeded random sampling. Use `config.with_scheduler(SchedulerMode::Pct { depth: 3 })` for probabilistic concurrency testing (PCT), or `config.with_scheduler(SchedulerMode::Dfs)` for bounded depth-first search (DFS).

| Mode | What it explores | How exploration ends when executions pass |
| --- | --- | --- |
| `Random` | Samples scheduling choices among runnable tasks using `seed`. | Runs `iterations` executions. |
| `Pct { depth }` | Samples task priorities using `seed`. After calibration, each execution has at most `depth - 1` sampled priority changes. Explicit yields can also lower priorities. | Runs `iterations` executions, including calibration. Stops after calibration if that execution offered no task choices. |
| `Dfs` | Enumerates modeled task choices. Each execution starts with the same fixed random data stream. | `record.dfs_status()` gives `Some(DfsStatus::Complete)` after all reachable schedules pass, or `Some(DfsStatus::IterationLimit)` if schedules remain when the execution budget runs out. |

`iterations` limits executions. `max_steps` limits scheduling operations in each execution and returns `FailureKind::StepLimit` on exhaustion. Both must be positive, as must PCT `depth`. Random and PCT sample schedules. DFS covers the modeled task choices for the supplied fixture and fixed data stream, so vary the fixture separately to cover other inputs.

## Replay a failure

On failure, the harness prints a JSON replay record. It contains the scheduler mode and its bounds, the seed, the one-based iteration, the pinned engine versions, and the exact schedule. The record omits user payloads.

1. Save `failure.replay()` with `Replay::save`.
2. Load it with `Replay::load`.
3. Pass it and the same factory to `testing::replay`.

Single-record replay requires an exact schedule. Keep the scheduling behavior, dependencies, and fixture inputs compatible. `Replay::load` reports incompatible metadata by field. The current format is version 4, so regenerate records from earlier versions.

## Run a regression corpus

Save failure records for one scenario as test fixtures and pass their paths and the current scenario factory to `testing::replay_corpus`.

```rust
testing::replay_corpus(
    ["tests/schedules/lost-update.json", "tests/schedules/late-wakeup.json"],
    scenario,
).unwrap();
```

Each run starts with fresh fixture state and must follow the entire recorded prefix. After the prefix, round-robin scheduling lets a fixed scenario run past the previous failure while the recorded step limit still applies. The first incompatible file, changed prefix, or failed execution ends the corpus run with the path and cause.

## What the harness schedules

`Actor`, `Channel`, `Fanout`, `Reply`, `Watch`, `SingleFlight`, `Tasks`, and `Cancellation` support deterministic scheduling.

Channels and watches:

- The harness covers synchronous channel operations, competing senders and receivers, streams, close-and-drain, endpoint drops, and cancellation by drop of a pending future.
- `Channel`, `Fanout`, and `Watch` share a private notification boundary that registers and then checks again.
- The bounded storage of `Channel` is a concurrent ring in production and a locked FIFO in the harness. The notification, closure, and endpoint ownership logic is shared.
- Async channel operations and limit acquisitions share a budget within each poll of the test future, a `testing::parallel` worker, or a scoped task. Exhaustion yields before changing channel state or taking a place. Replay includes that scheduling point.
- The harness schedules registration, notification, polling, and cancellation handoff. It treats each event-listener operation as one atomic notification step. It does not model the internal atomics of event-listener.

`SingleFlight`:

- The harness schedules joining, leaving, the hand-off of the operation between callers, and the end of a flight. A call can arrive while the last caller of an earlier operation is still dropping it.
- The operation's waker never switches tasks. It marks the operation woken and notifies the callers, which take turns polling it.

Tasks:

- `Tasks` keeps its rules for result claims, the oldest unclaimed error, panic precedence, and abort-and-drain cleanup. This includes concurrent maps and the ordered and unordered streams.
- Blocking work models the boundary between queued and started. A queued closure can be aborted. A started closure finishes before a scope joins.
- Replace real blocking I/O with deterministic fixtures that use the modeled primitives.

Time:

- Operations that take a cancellation accept virtual deadlines built from `time::now()`.
- Dropping a sleep removes its timer.
- A sleep belongs to the execution in which it is first polled. The harness rejects a pending sleep that is carried into another execution.
- The clock origin is opaque and shared within a process. A replay in another process keeps the elapsed virtual durations.
- An `Instant` carries no execution identity. Build deadlines inside each execution of the factory.
- `time::wall_now()` starts every execution at `Config::wall_epoch`, 2026-01-01T00:00:00Z by default, and advances in lockstep with `time::now()`. A `Replay` records the epoch and a replay reuses it, so `exp` checks against absolute timestamps repeat exactly.

Randomness:

- `adamantium::random` draws from the scheduler. The seed decides the draws, and a `Replay` records each one and repeats it.

## Rules for tests

- Use `#[test]`, not `#[tokio::test]`.
- Create primitives inside the execution that uses them.
- Draw random numbers from `adamantium::random`. Read time from `time::now()` and `time::wall_now()`. Replace I/O, other clocks, other randomness, and foreign executors with deterministic fixtures.

## Limits

- OS signal handling is outside the harness.
- The harness cannot diagnose every foreign future.
- A step bound cannot interrupt a loop that makes no scheduling calls.
