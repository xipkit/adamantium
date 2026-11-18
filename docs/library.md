# The library

## Values

`Str`, `Bytes`, `Array<T>`, `Deque<T>`, `Map<K, V>`, and `Set<T>` use copy-on-write storage. A change to a clone does not affect the original.

- `Str`, `Bytes`, and `Array` use `Deref` for reads through `str` or slice views.
- `Deque` uses `Deref` to `VecDeque`. It supports push and pop at both ends.
- `Map` has no `Index` implementation. A write that takes a position returns `None` or returns the value. It does not panic.
- These types are `Send + Sync` when their element types are.
- With the `serde` feature, these types support serialization.

### `Set`

`Set<T>` stores unique values in insertion order.

- A removal does not change the order of the other values.
- Two sets are equal when they have the same members, in any order.
- Union, intersection, difference, and symmetric difference borrow values in the documented set order. To make an owned set, collect the iterator.
- With `serde`, a set serializes as a sequence in insertion order. On deserialization, a duplicate value is stored one time.

### `Bytes` with serde

With `serde`, `Bytes` accepts a sequence of numbers in human-readable formats and a byte string in binary formats. It rejects JSON strings.

A binary byte string currently fails to deserialize when Serde buffers it. Serde buffers it for these attributes:

- `#[serde(flatten)]`
- `#[serde(untagged)]`
- Internally tagged enums (`#[serde(tag = "...")]`)
- Adjacently tagged enums (`#[serde(tag = "...", content = "...")]`), when the content comes before the tag

The buffer of Serde reports that it is human-readable, so `Bytes` requests a sequence and not a byte string. This choice keeps support for text formats such as quick-xml, csv, and envy, whose `deserialize_any` does not produce a sequence.

## Tasks

`Tasks<E>` manages a scope of tasks.

- `spawn` starts a task. `value` waits for its result.
- `spawner` gives children a cloneable handle to add siblings to the same scope.
- `spawn_handled` starts a task whose error goes to a handler instead of the scope. See [Handled tasks](#handled-tasks).
- `join` and `Tasks::run` wait for the remaining tasks.
- `join` returns the task errors that nothing observed.
- If the body or an unobserved task returns an error, the scope aborts the remaining tasks.
- In `Tasks::run` and `Tasks::run_until`, a task error that nobody can claim stops the body at its next `await`. An error is unclaimable once the task has failed and its handle has been dropped, in either order. The scope then aborts and drains the other tasks and returns the oldest unclaimed error. A handle that the body keeps can still claim its error, so that error waits until the handle is passed to `value` or dropped.
- A task panic resumes in the scope. In `Tasks::run` and `Tasks::run_until`, a task panic also stops the body at its next `await`. The scope then aborts and drains the other tasks and resumes the panic.
- Finished task entries are reaped without waiting when new async or blocking work is spawned. In `Tasks::run` and `Tasks::run_until`, they are also reaped as soon as the task finishes, even if the body spawns nothing more. `len()` counts unfinished tasks and `is_empty()` reports whether all work has finished; neither waits for joining.
- When the error type cannot be inferred, specify it with `Tasks::<E>::run`. This occurs when all errors pass through `?`.

### Discovering more work

Use `tasks.spawner()` when a task discovers work that should join the same
scope, such as a crawler finding links. `Spawner::spawn` takes `&self` and
returns `Result<Task<T, E>, SpawnError>`. Its task handles and errors follow
the same rules as `Tasks::spawn`; only the scope owner can call `value` or join.

A successful body return begins joining. Registered children may continue
adding siblings until the last registered task is drained. Keeping an idle
spawner does not keep the scope alive. Registration and closure are atomic:
each racing spawn either registers work that the scope joins, or returns
`SpawnError` and drops the future without polling it.

Failure shutdown, cancellation, deadlines and owner drop close shared admission
before aborting. Surviving handles stay closed. Rejected futures are destroyed
outside the admission lock, including cleanup that tries to spawn again.
As with other scope work, dropping the owner cannot wait for cleanup or stop
already running blocking closures.

The handle does not bound concurrency or deduplicate work. Keep spawning in
the body and send discoveries through a channel when a central coordinator
is useful for those policies. See `cargo run --example discovery` for a
self-contained graph traversal using a shared spawner.

### Handled tasks

`tasks.spawn_handled(future, on_error)` starts a task whose error goes to `on_error` and not to the scope. Use it for work whose failure concerns only that work, such as one connection of a server.

```rust
use adamantium::Tasks;

Tasks::run_until(shutdown, None, async |tasks| {
    loop {
        let (stream, peer) = listener.accept().await?;
        tasks.spawn_handled(serve(stream), move |error| {
            eprintln!("connection from {peer} failed: {error}");
        });
    }
})
.await
```

- `on_error` runs in the task after the future fails. Log the error, count it, or drop it.
- The error does not stop the body, does not abort the other tasks, and does not fail `join`.
- The task has no handle. Its error type can be different from the error type of the scope.
- A panic in the future or in `on_error` resumes in the scope, as for any task. In `run` and `run_until`, the panic stops the server when the task ends, not at shutdown.
- The scope aborts a handled task like any other task. An aborted task does not call `on_error`.
- `tasks.spawn_handled_named(name, future, on_error)` gives the task a name for the `tracing` feature. `on_error` runs inside the span of the task. See [Task names and tracing](#task-names-and-tracing).

`cargo run --example server --features signal` serves local TCP connections under a concurrency limit and shuts down on a signal with a deadline. One connection fails, its error goes to the handler, and the server continues to serve the rest.

### Which spawn to use

| Spawn | Use it when | A task error |
| --- | --- | --- |
| `spawn` | The body needs the result, or the work cannot fail alone. Most fork and join work is like this. | Fails the scope. |
| `spawn_handled` | A failure concerns only that task, such as one connection of a server. | Goes to `on_error`. The scope continues. |
| `spawn_supervised` | A long-lived worker must restart after a failure. | Restarts the task. When no restart is left, fails the scope. |
| `spawn_blocking` | Blocking work that the scope must wait for. | Fails the scope. |

### `concurrently`

`concurrently` maps items in parallel with a concurrency limit. It returns the results in input order.

- It starts on the first poll.
- It takes an item only when a place is free. At most `limit` futures exist at one time.
- A failure stops it from taking more items.
- The iterator and the closure must be `Send`. They run inside the poll of the returned future.

### Supervised tasks

`tasks.spawn_supervised(restarts, work)` starts a task that restarts when it fails. Use it for a long-lived worker, such as a connection that can drop.

```rust
use adamantium::{Backoff, Restarts, Tasks};
use std::time::Duration;

let restarts = Restarts::new(5, Duration::from_secs(60))
    .backoff(Backoff::exponential(Duration::from_millis(100), Duration::from_secs(5)));

Tasks::run(async |tasks| {
    let worker = tasks.spawn_supervised(restarts, |error| async move {
        if let Some(error) = error {
            eprintln!("restarting after: {error}");
        }
        serve().await
    });
    tasks.value(worker).await
})
.await?;
```

- `work` makes the future of the task. The first call gets `None`.
- When the future returns `Err`, `work` is called again with that error. Log the error there, and start the new attempt from clean state.
- When the future returns `Ok`, the task ends with that value. It does not restart.
- A panic does not restart the task. The scope resumes the panic.
- The scope aborts a supervised task like any other task. It does not wait for a restart.

`Restarts::new(limit, per)` allows at most `limit` restarts in any window of length `per`.

- A restart that is older than the window does not count. A task that fails occasionally continues to run.
- A task that fails with no restart left ends with that error. The scope handles the error like the error of any other task.
- `.backoff(backoff)` adds a wait before each restart. Each further restart in the same window waits longer. Only the delays of the `Backoff` apply. Its limit on attempts does not.

`tasks.spawn_supervised_named(name, restarts, work)` gives the task a name for the `tracing` feature. See [Task names and tracing](#task-names-and-tracing).

`retry` is for one operation that must succeed one time. `spawn_supervised` is for a task that runs for a long time and can fail many times.

### `first_ok`

`first_ok` takes items in the same way as `concurrently`. It returns the first success, then aborts and drains the items that still run.

- A failure only frees a place for the next item.
- If every item fails, it returns all the errors in input order.
- A limit of one tries the items in sequence. A limit equal to the input length races them.

### `blocking`

`blocking` runs a closure on the blocking pool. A panic in the closure resumes on the calling task.

WASI has one thread. On WASI, `blocking` and `Tasks::spawn_blocking` run the closure on that thread, and other tasks wait until the closure returns.

### `race`

`race` waits for the first of several futures and returns its output. Use it in place of `select!` to wait for a message, a timer, or a shutdown request.

```rust
use adamantium::{Channel, race};

enum Event {
    Job(Option<u32>),
    Stop,
}

let event = race((
    async { stop.receive().await; Event::Stop },
    async { Event::Job(jobs.receive().await) },
))
.await;
```

- `race` takes a tuple of two to eight futures. The futures must have the same output. To wait for different events, make each future return one variant of an enum.
- The futures run inside `race`, on the task that awaits it. They can borrow from the caller, and they do not have to be `Send` or `'static`.
- `race` needs no runtime. It works without the `tokio` feature.
- `race` polls the futures in the order written. If two futures are ready at the same time, the earlier one wins.
- When one future finishes, `race` drops the others at the `await` where they wait. Race only futures that lose nothing when dropped, such as a receive or a sleep.
- To run work in parallel, or to keep the first success and ignore failures, use `first_ok`.

### `until`

With the `tokio` feature, `until(&cancellation, deadline, future)` waits for one drop-safe future until `cancellation` is requested or `deadline` passes (`None` for no deadline).

- It returns `Ok(output)` if `future` finishes first, or `Err(UntilError::Cancelled)` / `Err(UntilError::DeadlineExceeded)` if shutdown wins.
- As with `receive_until` and `Tasks::run_until`, a request already present wins over a ready future, and cancellation wins over an equally ready deadline.
- When it stops early, `until` drops `future` at its current `await`. Pass only futures that lose nothing when dropped, such as `Channel::receive`, `Receiver::receive`, `ReplyReceiver::receive`, `Reply::closed`, `Feed::receive`, `Watcher::receive`, `Watcher::wait_for`, `time::sleep`, `time::sleep_until`, or `Cancellation::cancelled`. Do not pass `send` futures (use `send_until`).

## Actors

### `Actor`

`Actor<S>` shares state between tasks.

- `with` gives exclusive access. `read` gives read-only access.
- Both take synchronous closures, so neither holds the lock across an `await`.
- You can pass a read-only method such as `Tally::total` to `read`.
- A call to `with` or `read` from inside either method, on any actor, panics. This prevents deadlocks.

Use `Actor::with` for synchronous updates of shared state.

### `AsyncActor`

`AsyncActor<M>` sends typed messages to one worker inside `Tasks`. The worker owns its state. It awaits each handler before it takes the next message. The state thus has exclusive access through I/O without a lock.

Use `AsyncActor` when the state owns I/O that must not interleave, such as a connection with a protocol exchange in progress:

```rust
use adamantium::{AsyncActor, Tasks};

#[tokio::main]
async fn main() -> Result<(), &'static str> {
    Tasks::run(async |tasks| {
        let actor = AsyncActor::new(tasks, Vec::new(), 4, |state, message: u64| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                state.push(message);
                Ok(())
            })
        });
        actor.send(42).await.map_err(|_| "closed")?;
        actor.close();
        Ok(())
    }).await
}
```

The handler:

- The handler returns a boxed `Send` future that borrows `&mut state`.
- The state and the messages need `Send + 'static`. They do not need `Clone` or `Sync`.
- A handler must not wait for its own worker to advance, directly or through another mailbox. Mailbox cycles can deadlock.

The mailbox:

- The mailbox is bounded, so it applies backpressure.
- The capacity limits the queued messages, plus the one that is being handled. It does not limit the producers that wait. A capacity of zero means one.
- Concurrent senders are ordered only by the order in which their messages enter the queue.
- A successful `send` means that the mailbox accepted the message. It does not mean that the message was processed or durably committed.
- A send to a closed mailbox returns the original message.
- A message type can have a reply channel as a field. The mailbox has no receive operation.

Close and failure:

- Call `close()` before the scope completes successfully. `close()` rejects new sends and drains the accepted work. Dropping the mailbox handles does not close the mailbox.
- A handler error closes the mailbox and discards the queued messages. The error fails the scope through `Tasks`. Nothing holds the worker's handle, so under `Tasks::run` or `Tasks::run_until` the error also stops a pending scope body.
- A panic or a cancellation also closes the mailbox and discards the queue. This is true before the first poll of the worker, and while mailbox handles remain.
- Cancellation can interrupt I/O. It cannot roll back external effects or guarantee delivery.
- To wait for cleanup, await the shutdown of the scope. Dropping a scope requests aborts but does not wait.

`cargo run --example async_actor` drives a real local TCP echo connection, checks explicit replies, then closes and drains the mailbox and the server.

## Channels

`Channel<T>` is a bounded queue. Its clones can send and receive.

- `split()` consumes the channel and returns a `Sender<T>` and a `Receiver<T>`. Both can be cloned.
- When the last sender is dropped, the channel closes. Receivers drain the queued messages and then receive `None`.
- When the last receiver is dropped, pending sends wake with `Closed(value)`.
- A combined `Channel` clone owns both endpoints. Drop or split every such clone before you rely on closure by endpoint drop.
- Either endpoint, or a combined handle, can also call `close()`.

### Operations that do not wait

`Sender::try_send` and `Channel::try_send` return `TrySendError::Full(value)` or `TrySendError::Closed(value)` without waiting.

`Receiver::try_receive` and `Channel::try_receive` return one of these:

- `Ok(value)`
- `Err(TryReceiveError::Empty)` while the channel is open and empty
- `Err(TryReceiveError::Closed)` after the channel is closed and drained

These operations work with `default-features = false`, without Tokio:

```rust
use adamantium::{Channel, TryReceiveError};

let (sender, receiver) = Channel::bounded(2).split();
sender.try_send(42).unwrap();
drop(sender);
assert_eq!(receiver.try_receive(), Ok(42));
assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));
```

### Operations with a cancellation and a deadline

With the default `tokio` feature, `receive_until(&cancellation, deadline)` and `send_until(&cancellation, deadline, value)` wait only until the `Cancellation` is requested or the absolute deadline passes. Pass `None` for no deadline.

- A receive ends with `ReceiveError::Closed`, `Cancelled`, or `DeadlineExceeded`. When it ends early, it leaves the queued messages in place.
- A send ends with `SendError::Closed(value)`, `Cancelled(value)`, or `DeadlineExceeded(value)`. Every failure returns the value.
- As with `Tasks::run_until`, a request that is already present wins over a ready channel.
- Cancellation wins over a deadline that is ready at the same time.

### Receiver as a stream

With the `tokio` feature, `Receiver<T>` implements `futures_core::Stream<Item = T>`. It works with the concurrent streams through combinators such as `zip`.

`Receiver<T>` is not `Unpin`. Use `std::pin::pin!` before you call `StreamExt::next`.

## Topic

`Topic<T>` sends each message to every subscriber. No subscriber misses a message.

```rust
use adamantium::Topic;

let mut prices = Topic::new(8);
let display = prices.subscribe();
let log = prices.subscribe();

assert_eq!(prices.send(42).await, 2);
assert_eq!(display.receive().await, Some(42));
assert_eq!(log.receive().await, Some(42));
```

- Each subscriber is a `Receiver<T>` with its own queue of `capacity` messages.
- `send(value)` waits until every subscriber has room. Then it gives each subscriber a clone of the value. It returns the number of subscribers that received the value.
- A slow subscriber slows the topic down. Use `send_until` to stop the wait on a `Cancellation` or at a deadline.
- If a send stops before delivery, no subscriber receives the value. `send_until` returns the value in the error.
- Every subscriber receives the messages in the order they were sent.
- A subscriber receives the messages sent after it subscribed.
- Dropping a `Receiver` removes that subscriber.
- Dropping the topic closes every subscriber. Each one receives its queued messages, then `None`.

A topic has one sender. `send` takes `&mut self`, so two sends cannot overlap. To publish from several tasks, send to one task that owns the topic through a `Channel`.

`topic.subscriptions()` returns a `Subscriptions<T>` handle. Clone it and call `subscribe()` to add subscribers from other tasks.

`Topic` works with `default-features = false`. `send_until` needs the `tokio` feature.

## Fanout

`Fanout<T>` sends each event to every subscriber without waiting. A subscriber that falls a whole queue behind is disconnected, so it never holds up the others.

```rust
use adamantium::{Ended, Fanout, Sent};

let prices = Fanout::new(2);
let mut display = prices.subscribe();
let mut stalled = prices.subscribe();

for price in [1, 2] {
    prices.send(price)?;
    assert_eq!(display.try_receive(), Ok(Some(price)));
}
assert_eq!(prices.send(3)?, Sent { delivered: 1, disconnected: 1 });
assert_eq!(display.try_receive(), Ok(Some(3)));

assert_eq!(stalled.try_receive(), Ok(Some(1)));
assert_eq!(stalled.try_receive(), Ok(Some(2)));
assert_eq!(stalled.try_receive(), Err(Ended::Overflowed));
```

- Each subscriber is a `Feed<T>` with its own queue of `capacity` events.
- `send(value)` never waits. It gives a clone of the value to every feed with room. It disconnects every feed whose queue is full. It returns a `Sent` that counts both.
- A disconnected feed receives the events already in its queue, then `Err(Ended::Overflowed)`. It never receives a later event, so what a feed receives has no gaps.
- `Fanout` is `Clone`. Every clone can send and subscribe, and every feed receives the sends of all clones in the same order.
- A feed receives the events sent after it subscribed. Dropping a `Feed` unsubscribes it.
- `close()`, or dropping the last `Fanout`, closes the fanout. Each feed receives its queued events, then `Err(Ended::Closed)`, and later sends return the value in `Closed`. Feeds don't hold a fanout open.
- `receive_until` stops waiting on a `Cancellation` or at a deadline.

To recover from `Ended::Overflowed`, call `feed.resubscribe()` first, then read a snapshot of the state the events describe, then apply the new feed's events. Subscribing first means no event falls between the snapshot and the feed. The new feed can repeat a change the snapshot already has, so applying an event twice must do no harm.

Use `Topic` when the sender should wait for a slow subscriber instead, and `Watch` when only the latest value matters.

`Fanout` works with `default-features = false`. `receive_until` needs the `tokio` feature.

## Reply

`Reply::pair()` makes a one-shot `Reply<T>` and the `ReplyReceiver<T>` that waits for it. Use the pair for a request and a reply between tasks.

- `send(value)` consumes the reply and never waits. If the receiver is gone, it returns the value in `Closed(value)`.
- `receive()` returns the value. It returns `None` if the reply was dropped without a send.
- The side that replies can check `is_closed()` or await `closed()`. It can then stop work whose answer nobody wants.

## Watch

`Watch<T>` holds the latest value of some state, such as configuration.

- `send(value)` replaces the value and wakes every `Watcher<T>`.
- `subscribe()` makes a `Watcher<T>`.
- `receive()` on a watcher waits for a value that the watcher has not seen and returns a copy. It skips the values sent between.
- `wait_for(condition)` waits for a value that satisfies `condition`. It starts with the current value.
- `get()` reads the current value.
- All clones of a `Watch` can send.
- The watch closes when the last clone is dropped or when `close()` is called. Each watcher then receives the last value, if it has not seen it, and then `None`.

`Reply` and `Watch` both work with `default-features = false`. Both receivers also have `try_receive()`. With the `tokio` feature, both have `receive_until(&cancellation, deadline)`, which follows the rules of the channel.

## Shutdown and cancellation

For explicit shutdown, use `Tasks::run_until(cancellation, deadline, body)` or `tasks.join_until(cancellation, deadline, body_result)`.

### The signal and the deadline

`Cancellation::new()` creates a signal that can be cloned.

- A caller requests shutdown with `cancel()`.
- Async work can await `cancelled()`.
- Blocking work can check `is_cancelled()`.
- Use a new signal for each scope. The scope also sets the signal on failure, deadline, completion, or drop.

The deadline is an optional `tokio::time::Instant`. Build it from `adamantium::time::now()`. `None` needs no timer.

### Results

These methods return `ScopeError::Failed(error)`, `Cancelled`, or `DeadlineExceeded`.

- A completed body error wins over an unclaimed child error, which wins over shutdown.
- A child panic stops the body at its next `await`, as a child error that nobody can claim does. The panic resumes after the scope drains its tasks.
- If cancellation and the deadline are both ready, cancellation wins.
- After shutdown starts, its reason stays the same during cleanup.
- A scope that is already cancelled or expired does not poll its body.
- As with `run`, a child error that nobody can claim stops the body and cancels the signal. If a shutdown request is ready at the same poll, shutdown still starts cleanup, but the child error is the result.

### Cleanup

- Cancellation drops the body, then aborts and drains the async children before it returns.
- Synchronous destructors finish. Asynchronous cleanup is not guaranteed to run.
- Use `tasks.spawn_blocking` for blocking closures that the scope must wait for. Let them check the shared signal between bounded operations.
- Blocking work that has started cannot be stopped by force. Cleanup can thus continue after the deadline.
- The free `blocking` function detaches its started closure if its future is dropped. This includes the case where an enclosing async task is aborted.
- Dropping a scope future, or a panic in its body, signals shutdown and requests aborts. It cannot wait for cleanup. To wait for cleanup, request cancellation and continue to await the scope.

### OS signals

The optional `signal` feature adds `cancellation.cancel_on_signal().await`. It requests cancellation on SIGINT or SIGTERM on Unix, and on Ctrl-C only on Windows.

Await it together with your scope in `tokio::try_join!`:

```rust
use adamantium::{Cancellation, Tasks};

let cancellation = Cancellation::new();
let (_, outcome) = tokio::try_join!(
    cancellation.cancel_on_signal(),
    async {
        Ok::<_, std::io::Error>(Tasks::run_until(
            cancellation.clone(), None, async |_| serve().await,
        ).await)
    },
)?;
// `outcome` is the scope's result, including `ScopeError::Cancelled` on a signal.
```

- The future also finishes when the scope cancels the signal on normal completion.
- A registration error is returned as `std::io::Error`.
- It spawns no background task.
- It starts to listen on the first poll. Dropping the future stops the listening and does not cancel.
- The process handlers that Tokio installs stay in place afterward. The default behavior of a signal, which is to end the process, is not restored.

## Time

With the `tokio` feature, use `adamantium::time::sleep(duration)` and `adamantium::time::sleep_until(deadline)` for waits.

- Outside the deterministic harness, these functions and `time::now()` use the Tokio clock, including its paused test clock.
- Inside a deterministic execution, they use virtual time without real timers. Time advances to the earliest pending deadline only when no application task can run. All timers at that instant become ready together.
- For periodic waits, use a loop over `time::sleep_until`.
- To measure elapsed wall-clock time, `std::time::Instant::now()` is still allowed.

## Task names and tracing

Use the optional `tracing` feature to find work that does not finish. Start tasks with `tasks.spawn_named("fetch", future)`, `tasks.spawn_blocking_named("parse", closure)`, or `tasks.spawn_handled_named("connection", future, on_error)`.

- Each named task runs inside an `adamantium.task` tracing span. The span has a `task.name` field, and its parent is the current span.
- When the deadline expires, the scope emits a warning with `running_tasks`. This is a snapshot of the unfinished named tasks in spawn order. The scope emits it before it requests cancellation or waits for cleanup.
- The snapshot includes queued work and started blocking closures. It excludes completed tasks, including tasks whose handles were never awaited.
- Duplicate names are kept.
- `tasks.spawn_supervised_named("worker", restarts, work)` names a supervised task. All its starts run inside one span, and the snapshot includes the task until it ends. Each restart emits a warning in that span with `restart`, the number of the restart in the current window, and `delay`, the wait before it.
- To collect the spans and warnings, install a tracing subscriber in the application.

Unnamed tasks create no spans and no diagnostic records.

Without `tracing`, the named methods behave as the unnamed methods do. They do not convert or store names, and the library has no tracing dependency.

## Limit

`Limit::new(places)` lets at most `places` calls run at the same time. Clones share the limit, so give a clone to each task that must count against it.

```rust
use adamantium::Limit;

let connections = Limit::new(4);
let rows = connections.run(|| query(7)).await;
```

- `run(work)` waits for a place, runs the work, and returns its output. It calls `work` only when a place is free.
- The place becomes free when the work finishes or when the future is dropped.
- Waiting calls do not get places in the order of their arrival.
- `run_until(&cancellation, deadline, work)` stops the wait or the work on a `Cancellation` or at a deadline. It returns `ScopeError::Failed(error)`, `Cancelled`, or `DeadlineExceeded`. It needs the `tokio` feature.
- `available()` returns the number of free places.

A call to `Limit` inside another `Limit` panics before waiting. This covers taking the same limit twice and taking two different limits in either order.

To limit the items of one collection, use `concurrently`. It needs no shared limit.

### Per-key exclusion

`KeyedLimit::new()` lets one call run per key. Clones share the table. A server can use a connection limit around a per-session refresh. Re-read the session inside the keyed work so a waiting request sees the token its predecessor rotated.

```rust
let connections = Limit::new(3);
let sessions = KeyedLimit::new();
connections.run(|| sessions.run(id, || refresh_if_stale(id))).await;
```

`run(key, work)` releases the key on completion or drop. Dropping a waiter gives up its wait. The table evicts each key when its final holder or waiter leaves. With Tokio, `run_until(key, &cancellation, deadline, work)` applies the same cancellation rules as `Limit::run_until`.

Keyed work may run on its own or under a `Limit`. While holding a key, entering any `Limit` or `KeyedLimit` panics, even for a different key or table. The check covers directly polled work. A holder must also avoid awaiting a spawned task that needs a resource it holds.

## Retries and rate limits

### `retry`

`retry(&cancellation, deadline, backoff, attempt)` calls `attempt` until its future succeeds. Between failures, it waits for the time that a `Backoff` gives.

- `Backoff::exponential(first, max)` doubles the wait, up to `max`.
- `Backoff::constant(delay)` keeps the wait fixed.
- `.attempts(n)` limits the number of attempts. Without it, only the cancellation and the deadline end the retries.
- `retry_if` takes a predicate that decides which errors get another attempt.

### `RateLimit`

`RateLimit::new(permits, per)` lets at most `permits` calls start in any window of length `per`. Clones share the limit, so one clone for each task limits a whole `concurrently` map:

```rust
let limit = RateLimit::new(10, Duration::from_secs(1));
let pages = concurrently(urls, 8, |url| {
    let (limit, cancellation) = (limit.clone(), cancellation.clone());
    async move { limit.run_until(&cancellation, None, || fetch(url)).await }
})
.await?;
```

### Results

`retry` and `RateLimit::run_until` return `ScopeError::Failed(error)`, `Cancelled`, or `DeadlineExceeded`.

- Every wait inside them ends when the `Cancellation` is requested or the deadline passes. This applies to the wait between attempts and to the wait for room under the limit.
- An attempt or a call that still runs at that time is dropped.
- As with `Tasks::run_until`, a request that is already present wins. A cancelled call never starts its work and never takes room under the limit.
- Cancellation wins over a deadline that is ready at the same time.

## Streaming concurrent maps

`concurrently_ordered(items, limit, f)` yields results in input order. `concurrently_unordered(items, limit, f)` yields results in the order in which they complete. The order of results that complete at the same time is not specified.

Both return a `Send` stream of `Result<T, ScopeError<E>>`. Pin the stream and read it with `StreamExt::next` from `futures-util` or another compatible stream library:

```rust
use adamantium::concurrently_ordered;
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), adamantium::ScopeError<std::io::Error>> {
    let results = concurrently_ordered(0..100, 8, |n| async move {
        Ok::<_, std::io::Error>(n * 2)
    });
    let mut results = std::pin::pin!(results);
    while let Some(result) = results.next().await {
        println!("{}", result?);
    }
    Ok(())
}
```

`cargo run --example streaming` shows both result orders, a slow consumer, and explicit cancellation.

### The window

The window limits the items that are **started but not yet yielded** to `max(limit, 1)`. Running futures and buffered results share that limit.

- In the ordered variant, a slow first item stops the refill, even when later items finish.
- The unordered variant can yield those later results and refill their places.
- The ordered variant thus gives up some throughput for bounded storage.
- Memory is O(limit). This excludes the iterator of the caller, the individual values, and the results that the consumer keeps.
- Neither variant collects the input, and neither spawns a background producer.
- When the consumer pauses, no new items are scheduled. Tasks that have started can finish into the bounded buffer.

### Requirements

- Except for `into_iter`, the stream reads the input and calls the closure only during a poll.
- The iterator and the closure must be quick and `Send`. The input items do not have to be `Send`.
- The poll of the first window that is not empty needs a Tokio runtime.

### Shutdown

For shutdown, use `concurrently_ordered_until(items, limit, cancellation, f)` or `concurrently_unordered_until(...)`.

- These functions **observe** the signal that you supply. Completion, error, panic, and drop never cancel that signal or sibling streams.
- This is different from `Tasks::run_until`, which takes a new signal that the scope owns.
- A task can capture a clone of the signal to observe the shutdown of the parent.
- A cancellation request during a pause takes effect when polling resumes.

### Errors

When the stream observes an error or a cancellation:

1. It stops taking input.
2. It aborts and drains the async tasks that are still running.
3. It discards the buffered successes.
4. It yields one final error and then ends.

Precedence:

- A child panic resumes after the drain. It wins over the oldest recorded child error, which wins over `Cancelled`.
- An error does not wait for a slow earlier item, in either order.
- Results that the stream already yielded stay with the consumer.
- If the runtime cancels a task unexpectedly, the stream drains. It then panics if no child panic or error explains the missing result.

### Drop

- Dropping a pending `next()` while you keep the stream preserves its work.
- Dropping the stream requests aborts and does not wait. A panic in the iterator or the closure does the same.
- To wait for cleanup, request cancellation and continue to poll until the final error.
- Only async tasks are registered. Asynchronous cleanup is not guaranteed. Detached work, such as a started `blocking` closure, can continue after the stream ends.

### Dependencies

- Streaming uses `futures-core` for the standard `Stream` trait and `async-stream` for the state machine that yields. The `tokio` feature enables both.
- `futures-util` is a dev-dependency for the examples and tests. A consumer that uses its extensions must add it.
- With `default-features = false`, Tokio and `async-stream` are not included. `futures-core` is still present, because a dependency of the runtime-independent channel uses it.
