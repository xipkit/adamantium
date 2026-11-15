# Deterministic Actor tests

Enable `deterministic` in a test dependency to explore Actor interleavings with a
repeatable seed. Backend selection is per execution: ordinary application code
and Tokio tests continue to use production primitives in the same build.

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

The factory creates fresh state for every execution. The harness schedules
synchronous Actor calls and ownership operations too, so a separate `read`
followed by `with` can reveal a lost update even without an `await` between them.
On failure it prints a JSON replay record with the seed, failing iteration,
step bound, pinned engine versions and exact schedule, excluding user payloads.
Save `failure.replay()` with `Replay::save`, load it with `Replay::load`, and pass
it and the same factory to `testing::replay`. Keep code, dependencies and fixture
inputs unchanged when replaying. Deadlock and step exhaustion are failures;
passing exploration is not a proof of all executions.

Actor, Channel, Reply, Watch, Tasks and Cancellation support deterministic
scheduling, including synchronous
channel operations, competing senders and receivers, streams, close-and-drain,
endpoint drops and cancellation by dropping a pending future. Channel and Watch
share a private register-then-recheck notification boundary. Channel's bounded
storage uses a concurrent ring in production and a locked FIFO in the harness;
its notification, closure and endpoint ownership logic are shared. The harness schedules registration, notification,
polling and cancellation handoff, treating event-listener operations as atomic
notification steps rather than modeling event-listener's internal atomics.
Tasks preserve result claiming, oldest unclaimed errors, panic precedence and
abort/drain cleanup, including concurrent maps and ordered/unordered streams.
Blocking work models the queued/started boundary: queued closures can be
aborted, while started closures finish before a scope joins. Replace real
blocking I/O with deterministic fixtures using the modeled primitives.
`testing::parallel` also provides independent test workers. Cancellation-aware
operations accept virtual deadlines built from `time::now()`. Dropping a sleep
unregisters its timer. Sleeps belong to the execution where first polled;
carrying a pending sleep across executions is rejected. The opaque clock origin
is shared within a process, while replay across processes preserves elapsed
virtual durations. `Instant` itself carries no execution identity, so construct
deadlines inside each factory execution.
OS signal handling remains outside the harness. Use `#[test]`, not
`#[tokio::test]`, and create primitives inside the execution that uses them.
Replace I/O, real time, uncontrolled randomness and foreign executors with
deterministic fixtures. Arbitrary foreign futures cannot all be diagnosed, and
a step bound cannot interrupt a loop that makes no scheduling calls.
