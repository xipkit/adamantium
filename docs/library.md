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
- `join` and `Tasks::run` wait for the remaining tasks.
- `join` returns the task errors that nothing observed.
- If the body or an unobserved task returns an error, the scope aborts the remaining tasks.
- A task panic resumes in the scope.
- When the error type cannot be inferred, specify it with `Tasks::<E>::run`. This occurs when all errors pass through `?`.

### `concurrently`

`concurrently` maps items in parallel with a concurrency limit. It returns the results in input order.

- It starts on the first poll.
- It takes an item only when a place is free. At most `limit` futures exist at one time.
- A failure stops it from taking more items.
- The iterator and the closure must be `Send`. They run inside the poll of the returned future.

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
- A handler error closes the mailbox and discards the queued messages. The error propagates at scope join through `Tasks`. It does not interrupt a pending scope body.
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
- Child panics resume after the scope drains its tasks.
- If cancellation and the deadline are both ready, cancellation wins.
- After shutdown starts, its reason stays the same during cleanup.
- A scope that is already cancelled or expired does not poll its body.
- As with `run`, child failures are collected when the body asks for a value or joins. They do not interrupt a body that is otherwise pending.

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

Use the optional `tracing` feature to find work that does not finish. Start tasks with `tasks.spawn_named("fetch", future)` or `tasks.spawn_blocking_named("parse", closure)`.

- Each named task runs inside an `adamantium.task` tracing span. The span has a `task.name` field, and its parent is the current span.
- When the deadline expires, the scope emits a warning with `running_tasks`. This is a snapshot of the unfinished named tasks in spawn order. The scope emits it before it requests cancellation or waits for cleanup.
- The snapshot includes queued work and started blocking closures. It excludes completed tasks, including tasks whose handles were never awaited.
- Duplicate names are kept.
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

The work of one call must not start a second call on any limit. That call panics. A task that holds one place and waits for a second place can deadlock.

The check finds only a call that the work makes itself. It does not find a call in a task that the work spawns. Do not let such a task use a limit that the work holds.

To limit the items of one collection, use `concurrently`. It needs no shared limit.

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
