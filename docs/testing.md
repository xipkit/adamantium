# Deterministic Actor tests

The `deterministic` feature lets a test explore the interleavings of concurrent code from a seed. The same seed gives the same schedule.

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
- Deadlock and step exhaustion are failures.
- A test that passes is not a proof for all executions.

## Replay a failure

On failure, the harness prints a JSON replay record. The record has the seed, the failing iteration, the step bound, the pinned engine versions, and the exact schedule. It does not include user payloads.

1. Save `failure.replay()` with `Replay::save`.
2. Load it with `Replay::load`.
3. Pass it and the same factory to `testing::replay`.

Keep the code, the dependencies, and the fixture inputs unchanged for a replay.

## What the harness schedules

`Actor`, `Channel`, `Fanout`, `Reply`, `Watch`, `Tasks`, and `Cancellation` support deterministic scheduling.

Channels and watches:

- The harness covers synchronous channel operations, competing senders and receivers, streams, close-and-drain, endpoint drops, and cancellation by drop of a pending future.
- `Channel`, `Fanout`, and `Watch` share a private notification boundary that registers and then checks again.
- The bounded storage of `Channel` is a concurrent ring in production and a locked FIFO in the harness. The notification, closure, and endpoint ownership logic is shared.
- The harness schedules registration, notification, polling, and cancellation handoff. It treats each event-listener operation as one atomic notification step. It does not model the internal atomics of event-listener.

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

## Rules for tests

- Use `#[test]`, not `#[tokio::test]`.
- Create primitives inside the execution that uses them.
- Replace I/O, real time, uncontrolled randomness, and foreign executors with deterministic fixtures.

## Limits

- OS signal handling is outside the harness.
- The harness cannot diagnose every foreign future.
- A step bound cannot interrupt a loop that makes no scheduling calls.
