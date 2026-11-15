# The library

## Types

- **`Str`, `Bytes`, `Array<T>`, `Deque<T>`, `Map<K, V>`, and `Set<T>`** use copy-on-write storage, so changes to a clone do not affect the original. `Str`, `Bytes`, and `Array` use `Deref` for reads through `str` or slice views; `Deque` uses `Deref` to `VecDeque` and supports pushing and popping at both ends. `Map` has no `Index` implementation, and writes that take a position return `None` or return the value instead of panicking. These types are `Send + Sync` when their element types are and support serialization with the `serde` feature.
- **`Set<T>`** stores unique values in insertion order, removes values without reordering the rest, and compares membership regardless of order. Union, intersection, difference, and symmetric difference borrow values in the documented set order; collect an iterator to make an owned set. With `serde`, sets serialize as sequences in insertion order and deserialize duplicate values only once.
- **`Tasks<E>`** manages a scope of tasks. `spawn` starts a task, and `value` waits for its result. `join` and `Tasks::run` wait for remaining tasks. Unobserved task errors are returned at join; if the body or an unobserved task returns an error, the remaining tasks are aborted. Task panics resume in the scope. Specify the error type with `Tasks::<E>::run` when it cannot be inferred, such as when all errors pass through `?`.
- **`concurrently`** maps items in parallel with a concurrency limit and returns results in input order. It starts when first polled and takes each item only when a place opens, so at most `limit` futures exist at once and a failure stops it taking more. The iterator and closure must be `Send`, and they run inside the returned future's poll.
- **`first_ok`** takes items the way `concurrently` does and returns the first success, aborting and draining the items still running. A failure only frees a place for the next item; if every item fails, all the errors come back in input order. A limit of one tries the items in turn, and a limit as long as the input races them.
- **`blocking`** runs a closure on the blocking pool and resumes any panic on the calling task.

- **`Actor<S>`** shares state between tasks. `with` provides exclusive access and `read` provides read-only access through synchronous closures, so neither holds the lock across an await. A read-only method such as `Tally::total` can be passed to `read`. Calling `with` or `read` from inside either method on any actor panics to prevent deadlocks.
- **`AsyncActor<M>`** sends typed messages to a single worker inside `Tasks`. The worker owns its state and awaits each handler before taking the next message, allowing exclusive access through I/O without a shared-state lock. Its bounded mailbox applies backpressure. Call `close()` before successful scope completion to reject new sends and drain accepted work; dropping mailbox handles alone does not close it.
- **`Channel<T>`** is a bounded queue whose clones can send and receive. Consume it with `split()` to get separate clonable **`Sender<T>`** and **`Receiver<T>`** handles. Dropping the last sender closes the channel; receivers drain queued messages and then receive `None`. Dropping the last receiver wakes pending sends with `Closed(value)`. Any retained combined `Channel` clones still own both endpoints, so drop or split all of them before relying on endpoint-drop closure. Either endpoint or a combined handle can also call `close()` explicitly.

## Channels

With the `tokio` feature, `Receiver<T>` implements `futures_core::Stream<Item = T>`
and composes with the concurrent streams through combinators such as `zip`.
It is not `Unpin`; use `std::pin::pin!` before calling `StreamExt::next`.

`Sender::try_send` and `Channel::try_send` return `TrySendError::Full(value)` or
`TrySendError::Closed(value)` without waiting. `Receiver::try_receive` and
`Channel::try_receive` return `Ok(value)`, `Err(TryReceiveError::Empty)` while
open and empty, or `Err(TryReceiveError::Closed)` once closed and drained.
These channel operations work with `default-features = false`, without Tokio:

```rust
use adamantium::{Channel, TryReceiveError};

let (sender, receiver) = Channel::bounded(2).split();
sender.try_send(42).unwrap();
drop(sender);
assert_eq!(receiver.try_receive(), Ok(42));
assert_eq!(receiver.try_receive(), Err(TryReceiveError::Closed));
```

With the default `tokio` feature, `receive_until(&cancellation, deadline)` and
`send_until(&cancellation, deadline, value)` wait only until the
`Cancellation` is requested or the absolute deadline passes. Pass `None` for no
deadline. A receive ends with `ReceiveError::Closed`, `Cancelled`, or
`DeadlineExceeded`, and leaves queued messages in place when it ends early. A
send ends with `SendError::Closed(value)`, `Cancelled(value)`, or
`DeadlineExceeded(value)`, so the value comes back whichever way it fails. As
with `Tasks::run_until`, a request already present wins over a ready channel,
and cancellation wins over an equally ready deadline.

## Reply and Watch

`Reply::pair()` makes a one-shot **`Reply<T>`** and the **`ReplyReceiver<T>`**
that waits for it, for request and reply between tasks. `send(value)` consumes
the reply and never waits; it hands the value back in `Closed(value)` if the
receiver is gone. `receive()` returns the value, or `None` if the reply was
dropped without sending. The replying side can check `is_closed()` or await
`closed()` to stop work whose answer nobody wants.

**`Watch<T>`** holds the latest value of some state, such as configuration.
`send(value)` replaces it and wakes every **`Watcher<T>`** made with
`subscribe()`. A watcher's `receive()` waits for a value it hasn't seen and
returns a copy, skipping values sent in between; `wait_for(condition)` waits for
a value that satisfies `condition`, starting with the current one; `get()` reads
the current value. Clones of a `Watch` can all send. Dropping the last one, or
calling `close()`, closes it: each watcher receives the last value if it hasn't
seen it, then `None`.

Both work with `default-features = false`. Both receivers also have
`try_receive()` and, with the `tokio` feature, `receive_until(&cancellation,
deadline)`, which follows the channel's rules.

## Shutdown and cancellation

For explicit shutdown, use `Tasks::run_until(cancellation, deadline, body)` or
`tasks.join_until(cancellation, deadline, body_result)`. `Cancellation::new()`
creates a cloneable signal: callers request shutdown with `cancel()`, async work
can await `cancelled()`, and blocking work can check `is_cancelled()`. The deadline
is an optional `tokio::time::Instant`, built from `adamantium::time::now()`;
`None` needs no timer. Use a fresh signal per scope. The scope also signals it
on failure, deadline, completion, or drop.

Enable the opt-in `signal` feature to use
`cancellation.cancel_on_signal().await`: it requests cancellation on SIGINT or
SIGTERM on Unix, or Ctrl-C alone on Windows. Await it alongside your scope with
`tokio::try_join!`; it also finishes when the scope cancels the signal on normal
completion. Registration errors are returned as `std::io::Error`. No background
task is spawned. Listening starts on the first poll, and dropping the future
stops listening without cancelling. Tokio's installed process handlers persist
afterward, so the default signal termination behavior is not restored.

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

These methods return `ScopeError::Failed(error)`, `Cancelled`, or
`DeadlineExceeded`. A completed body error wins over an unclaimed child error,
which wins over shutdown; child panics are resumed after draining. If cancellation
and the deadline are both ready, cancellation wins. Once shutdown starts, its
reason stays fixed while cleanup runs. An already cancelled or expired scope
does not poll its body. As with `run`, child failures are collected when the body
asks for a value or joins; they do not interrupt an otherwise pending body.

Cancellation drops the body and aborts and drains async children before returning.
Synchronous destructors finish; asynchronous cleanup is not guaranteed to run.
Use `tasks.spawn_blocking` for blocking closures the scope must wait for, and let
them check the shared signal between bounded operations. Started blocking work
cannot be forcibly stopped, so cleanup can extend beyond the deadline. The free
`blocking` wrapper detaches its started closure if its future is dropped, including
when an enclosing async task is aborted. Dropping a scope future or panicking in
its body signals shutdown and requests aborts but cannot wait for cleanup. For
that guarantee, request cancellation and continue awaiting the scope.

## Time

With the `tokio` feature, use `adamantium::time::sleep(duration)` and
`adamantium::time::sleep_until(deadline)` for waits. These functions and
`time::now()` use Tokio's clock, including its paused test clock, outside the
deterministic harness. Inside an execution they use virtual time without real
timers: time advances to the earliest pending deadline only when no application
task can run, and all timers at that instant become ready together.
Build periodic waits with a loop over `time::sleep_until`. For measuring elapsed
wall-clock time, `std::time::Instant::now()` remains allowed.

## Task names and tracing

Enable the optional `tracing` feature and use `tasks.spawn_named("fetch", future)`
or `tasks.spawn_blocking_named("parse", closure)` to diagnose stuck work. Each
named task runs inside an `adamantium.task` tracing span with a `task.name` field
and the current span as its parent. On deadline expiry, the scope emits a warning
with `running_tasks`, a snapshot of unfinished named tasks in spawn order, before
requesting cancellation or waiting for cleanup. This includes queued work and
started blocking closures; completed tasks are excluded even if their handles
were never awaited. Duplicate names are retained. Install a tracing subscriber
in the application to collect the spans and warnings.

Unnamed tasks create no spans or diagnostic bookkeeping. Without `tracing`, the
named methods behave like their unnamed equivalents and do not convert or store
names; the library has no tracing dependency in that configuration.

## Retries and rate limits

`retry(&cancellation, deadline, backoff, attempt)` calls `attempt` until its
future succeeds, waiting between failures as a `Backoff` says:
`Backoff::exponential(first, max)` doubles the wait up to `max`, and
`Backoff::constant(delay)` keeps it fixed. `.attempts(n)` caps the attempts;
otherwise the cancellation and deadline are what end it. `retry_if` takes a
predicate that says which errors are worth another attempt.

`RateLimit::new(permits, per)` lets at most `permits` calls start in any
window of length `per`. Clones share the limit, so a clone per task bounds
the whole of a `concurrently` map:

```rust
let limit = RateLimit::new(10, Duration::from_secs(1));
let pages = concurrently(urls, 8, |url| {
    let (limit, cancellation) = (limit.clone(), cancellation.clone());
    async move { limit.run_until(&cancellation, None, || fetch(url)).await }
})
.await?;
```

Both return `ScopeError::Failed(error)`, `Cancelled`, or `DeadlineExceeded`.
Every wait inside them, between attempts or for room under the limit, ends as
soon as the `Cancellation` is requested or the deadline passes, and an attempt
or call still running is dropped. As with `Tasks::run_until`, a request already
present wins, so a cancelled call never starts its work or takes room under the
limit, and cancellation wins over an equally ready deadline.

## State that must await

Use `Actor::with` for synchronous shared-state updates. Use `AsyncActor` when
the state itself owns I/O that must not interleave, such as a connection with
an in-flight protocol exchange:

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

The handler returns a boxed `Send` future borrowing `&mut state`. State and
messages need `Send + 'static`, but not `Clone` or `Sync`. Capacity bounds
queued messages, plus one being handled; it does not bound waiting producers.
Zero capacity means one. Concurrent senders are ordered only by enqueue order.
A successful `send` means accepted, not processed or durably committed.
Closed sends return the original message. Reply channels can be fields in
message types; the mailbox does not expose a receive operation.

Handler errors close the mailbox, discard queued messages, and propagate at
scope join through `Tasks`. They do not interrupt a pending scope body.
Panics and cancellation also close and discard the queue, even before the
worker's first poll and with surviving mailbox handles. Cancellation can
interrupt I/O and cannot roll back external effects or guarantee delivery.
Await scope shutdown to wait for cleanup; simply dropping a scope requests
aborts without waiting. A handler must not wait on its own worker to advance,
directly or through another mailbox: mailbox cycles can deadlock.

`cargo run --example async_actor` drives a real local TCP echo connection,
checks explicit replies, then closes and drains the mailbox and server.

## Streaming concurrent maps

Use `concurrently_ordered(items, limit, f)` to consume results in input order,
or `concurrently_unordered(items, limit, f)` for observed completion order
(simultaneous completions have unspecified order). Both return a `Send` stream
of `Result<T, ScopeError<E>>`. Pin it and consume it with `StreamExt::next` from
`futures-util` or another compatible stream library:

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

`cargo run --example streaming` demonstrates both result orders, a slow
consumer, and explicit cancellation.

The window bounds **started but not yet yielded** items to `max(limit, 1)`.
Running futures and buffered results share that bound. A slow first ordered
item therefore stops refill even when later items finish; the unordered variant
can yield those later results and refill their slots. This trades ordered
throughput for bounded storage. Memory is O(limit), excluding the caller's
iterator, individual values, and results the consumer retains. Neither variant
collects the input or spawns a background producer. Pausing consumption stops
new scheduling; already started tasks may finish into the bounded buffer.
Apart from `into_iter`, input consumption and closure calls happen only during
polling. Both must be quick and `Send`; input items need not be `Send`. Polling
the first nonempty window needs a Tokio runtime.

For shutdown, use `concurrently_ordered_until(items, limit, cancellation, f)`
or `concurrently_unordered_until(...)`. These **observe** their supplied signal;
completion, error, panic, and drop never cancel it or sibling streams. This
differs from the fresh, scope-owned signal passed to `Tasks::run_until`.
Tasks can capture a clone to observe parent shutdown. A cancellation request
while consumption is paused takes effect when polling resumes.

On an observed error or cancellation, intake stops, outstanding async tasks
are aborted and drained, and buffered successes are discarded. The stream
yields one terminal error and then ends. A child panic is resumed after
draining and wins over the oldest recorded child error, which wins over
`Cancelled`. Errors bypass a slow earlier item in either order. Results already
yielded remain with the consumer. Unexpected runtime task cancellation drains
and panics if no child panic or error explains the missing result.

Dropping a pending `next()` while retaining the stream preserves its work.
Dropping the stream, or a panic in the iterator/closure itself, requests aborts
without waiting. To await cleanup, request cancellation and keep polling to the
terminal error. Only async tasks are registered: asynchronous cleanup is not
guaranteed, and detached work such as a started `blocking` closure can outlive
the stream.

Streaming uses optional `futures-core` for the standard `Stream` trait and
`async-stream` for the yielding state machine, both enabled by the existing
`tokio` feature. `futures-util` is a dev dependency for examples and tests;
consumers using its extensions add it themselves. Disabling default features
excludes Tokio and `async-stream`; `futures-core` is still used transitively by
the existing runtime-independent channel dependency.

## `Bytes` with serde

With `serde`, `Bytes` accepts numeric sequences in human-readable formats and
byte strings in binary formats; JSON strings are rejected. Binary byte strings
currently fail to deserialize when Serde buffers them for `#[serde(flatten)]`,
`#[serde(untagged)]`, internally tagged enums (`#[serde(tag = "...")]`), or
adjacently tagged enums (`#[serde(tag = "...", content = "...")]`) when the
content comes before the tag. Serde's buffer reports itself as human-readable,
so `Bytes` requests a sequence instead of a byte string. This hint preserves
support for text formats such as quick-xml, csv, and envy, whose
`deserialize_any` does not produce a sequence.
