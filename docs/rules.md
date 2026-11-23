# The rules

## Where the rules are

- The default lint levels are in the `[workspace.lints]` table of this repository.
- The default Clippy configuration is in [`cli/clippy.toml`](../cli/clippy.toml). The Cargo configuration of the repository points Clippy at that same bundled file.
- `cargo adamantium rules` prints both.
- The optional strict tier adds lint levels from `[workspace.metadata.adamantium-strict.lints]` and Clippy paths from [`cli/clippy-strict.toml`](../cli/clippy-strict.toml). `cargo adamantium rules --strict` prints the combined rules.

## Properties

The rules and the library give these properties to code that follows them:

| Property | Enforced by |
| --- | --- |
| No `unsafe` code | `unsafe_code = "forbid"` |
| Every value can cross a task | `future_not_send`. `Rc`, `RefCell`, `Cell`, and `LocalSet` are not allowed. The entry points of `LocalRuntime` and the methods that read or write a `thread_local!` value are not allowed, because a task can move to another thread at any `await`. |
| No task outlives its scope | These are not allowed: `tokio::spawn`, `spawn_local`, `JoinSet`, the spawn methods of Tokio's `Handle`, `Runtime`, `LocalRuntime`, and `task::Builder`, the task and builder spawns of async-std, `smol::spawn`, the executor spawns of smol, `std::thread::spawn`, and `scope`. Tasks use `Tasks`. |
| No lock is held across an await or inside another lock | `await_holding_lock`. `Mutex` and `RwLock` from std, tokio, parking_lot, and futures are not allowed. `Actor::with` is synchronous and rejects nesting. |
| Unbounded channel constructors restricted | The unbounded channel constructors from std, Tokio, async-channel, flume, and crossbeam-channel are not allowed. Use `Channel`. |
| No channel drops messages for a receiver that falls behind | Tokio's `broadcast` channel is not allowed. Use `Topic`, which waits until every subscriber has room, `Fanout`, which disconnects a subscriber that falls behind, or `Watch` when only the latest value matters. |
| No worker thread is blocked | These are not allowed: `std::thread::sleep`, `block_in_place`, every Tokio `spawn_blocking`, `async_std::task::spawn_blocking`, `smol::unblock`, and `block_on` on a `Handle` or `LocalRuntime`. Blocking work uses `blocking`. |
| No silently discarded must-use results or futures | `unused_must_use`, `let_underscore_must_use`, `let_underscore_future`. An intentional discard uses an explicit `drop`. |
| No panic from indexing, unwrapping, or an explicit `panic!` | `indexing_slicing`, `string_slice`, `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`. `Vec::remove`, `Vec::insert`, `Vec::swap_remove`, `slice::split_at`, and `slice::copy_from_slice` are not allowed. |
| No silent truncation or wrap | `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`. `overflow-checks = true` in release. |
| Explicit leak APIs and weak references restricted | `mem_forget`, `Box::leak`, `ManuallyDrop`, `Arc::downgrade`, and `Weak` are not allowed. |
| No skipped cleanup | `exit`. `std::process::abort` is not allowed. |

## Synchronization

Adamantium's synchronization primitives use the deterministic runtime's scheduler when a test runs under it. Use `Topic` to deliver every broadcast message to each subscriber with backpressure. Use `Fanout` when the sender must not wait, so a subscriber that falls behind is disconnected. Use `Watch` when subscribers only need the latest value.

| Primitive | Tier | Modeled replacement and behavior |
| --- | --- | --- |
| Bounded Tokio `mpsc` | Strict | Use `Channel::bounded(n).split()` for an async queue with backpressure. Adamantium receivers can be cloned to compete for work. Tokio has one receiver and serves sends and reservations in queue order. Adamantium has no reservation permits or admission-order guarantee. |
| Tokio `oneshot` | Default | Use `Reply::pair()`. Sending consumes the reply and returns the payload in `Closed<T>` if the receiver closed. Use `receive().await` for an `Option<T>` in place of awaiting the Tokio receiver for a `Result`. |
| Tokio `watch` | Default | Use `Watch` and `Watcher` for latest-value state. A watcher reads cloned snapshots. Adamantium replaces the whole value and retains updates even with zero watchers. Check any use of borrowed reads, in-place mutations, or Tokio's failed `send` when its last receiver goes away. |
| Tokio `Notify` | Strict | Use `Cancellation` for sticky shutdown, `Watch` for a change in state, or `Channel::bounded(1)` with `try_send(())` for a coalescing single-consumer signal. Handle a full channel as an already pending signal. Tokio's stored single permit and its wake-all operation for existing waiters have different delivery semantics, so choose by intent. |
| Tokio `Semaphore` | Strict | Use `Limit::run` for a shared concurrency limit, `Limit::weighted(n)` in place of `acquire_many(n)`, and `concurrently` for one batch. `Limit` scopes its places to one call and, like Tokio, serves waiting calls in the order they started waiting. A weight above the limit's total returns `Overweight` instead of waiting. Code that needs transferable or dynamically adjustable permits needs a scoped exception. |
| Tokio `OnceCell` | Strict | Use `Once` for a value that async work makes once and then keeps. `get_or_init`, `get_or_try_init`, and `get` keep their signatures. A `Once` is shared by cloning it, and its constructor isn't `const`, so a `static` cell becomes a field of the state the tasks share. `set`, `take`, `get_mut`, and `into_inner` have no counterpart. The strict tier reports the `OnceCell` and `Lazy` of `async_once_cell` too. When making the value never awaits, use `std::sync::OnceLock`. |
| `std::sync::mpsc::sync_channel` | Default | Use `Channel::bounded(n).split()` for positive-capacity queues with async send and receive. Std can block the caller and supports a zero-capacity rendezvous. Adamantium treats zero as one. Keep a genuine blocking rendezvous at a blocking boundary with a scoped exception. |

## Allowed, and why

### Semaphores

The default tier allows Tokio's `Semaphore`. A task can hold a permit across an `await`, because a permit guards no data.

- To limit how many items run at one time, use `concurrently` or the streaming variants.
- To share one limit between tasks, use `Limit`. Its work may enter one `KeyedLimit` for per-key exclusion. All other direct nesting of these limits panics before waiting.
- For a budget whose calls cost different amounts, such as bytes in flight, use `Limit::weighted`. Each call takes as many places as it costs.
- Do not acquire a second permit while you hold one. Two tasks that do so in opposite orders deadlock.

### Tests

Tests are exempt from the panic rules. `unwrap`, indexing, and `panic!` are allowed in `#[test]` functions and `#[cfg(test)]` modules.

Helper functions in integration test files depend on the Rust version:

- Before Rust 1.100, Clippy does not treat them as tests, so they follow the rules.
- From Rust 1.100, they are exempt too.

### Atomics and statics

Atomics, and `OnceLock` and `LazyLock` statics, are allowed.

- An atomic holds no guard across an `await` and cannot deadlock. Use it for a single counter or flag.
- Clippy cannot see a check-then-act sequence that uses two atomics. State whose parts must agree belongs in one `Actor`.
- A `OnceLock` or `LazyLock` static is set one time. Its contents can still change if they hold something like an `Actor`. That state is outside every scope, and every task and every test in the binary shares it.
- It is better to pass an `Actor` to the tasks that use it.

## Timeouts

The rules do not allow `tokio::time::timeout` or `tokio::time::timeout_at`. When the time expires, they drop the future in the middle of an `await`.

`race` with a timer (`time::sleep` or `time::sleep_until`) is also a timeout: when the timer wins, it drops the other future in the middle of an `await`.

- For scoped work, use `Tasks::run_until` with a `Cancellation` and a deadline.
- For channel operations, use `receive_until` and `send_until`.
- For a single drop-safe future (such as a receive, `Reply::closed`, `Watcher::wait_for`, or a socket read that loses nothing on drop), use `until` with a `Cancellation` and a deadline. Only race or pass to `until` futures that lose nothing when dropped.

## Selecting between futures

`futures::select!` is reported by `clippy::disallowed_macros`. For Tokio 1.44
and later, `clippy::disallowed_methods` reports `tokio::select!` through the
internal `tokio::macros::support::poll_budget_available` helper used by
invocations with future branches. An `else`-only invocation expands directly to
its fallback. The helper lets scoped exceptions work while Clippy's
[scoped macro allowances bug](https://github.com/rust-lang/rust-clippy/pull/17862)
affects a direct Tokio macro ban. Scope an exception to the `let` statement
containing the call.

- Use `first_ok` for the first successful task. It aborts and drains the remaining tasks.
- Use `Tasks::run_until` with `Cancellation` for scoped work that can be stopped.
- Use `receive_until` and `send_until` for channel operations with cancellation or a deadline.

## Limits

The rules have limits.

- **A plain `receive` or `send` can wait without a limit** while the channel stays open and empty, or open and full. Use `receive_until` or `send_until` to limit the wait with a `Cancellation` or a deadline.
- **Dependencies are not checked.** A dependency can still use unsafe code or panic. [`cargo adamantium deps`](cli.md#dependencies) shows which dependencies use `unsafe` or call disallowed paths.
- **Clippy cannot detect every call that blocks a worker thread.**
- **`Runtime::block_on` is allowed**, because `#[tokio::main]` and `#[tokio::test]` expand to it. A call inside a runtime panics at run time and does not block.
- **Arithmetic can panic in the default tier.** The strict tier reports arithmetic operators that can panic.
- **Neither tier prevents stack exhaustion** or panics inside called methods.

### Reference cycles

The rules do not detect or collect strong reference cycles.

- An `Actor` whose state owns a clone of itself, directly or through other actors, keeps every state in the cycle alive. This continues after the last external handle is dropped.
- The ban on `Weak` and `Arc::downgrade` does not prevent strong cycles.
- Keep actor ownership acyclic. Or remove the handles that form the cycle before you release the last external handle.
