# The rules

## Where the rules are

- The default lint levels are in the `[workspace.lints]` table of this repository.
- The Clippy configuration is in [`cli/clippy.toml`](../cli/clippy.toml). The Cargo configuration of the repository points Clippy at that same bundled file.
- `cargo adamantium rules` prints both.
- The optional strict rules are in `[workspace.metadata.adamantium-strict.lints]`. `cargo adamantium rules --strict` prints them. They do not apply to this repository.

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

## Allowed, and why

### Semaphores

Tokio's `Semaphore` is allowed. A task can hold a permit across an `await`, because a permit guards no data.

- To limit how many items run at one time, use `concurrently` or the streaming variants.
- To share one limit between tasks, use `Limit`. Its work cannot take a second place, so it cannot deadlock in that way.
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

The rules do not allow `tokio::time::timeout`. When the time expires, it drops the future in the middle of an `await`.

`race` with a timer (`time::sleep` or `time::sleep_until`) is also a timeout: when the timer wins, it drops the other future in the middle of an `await`.

- For scoped work, use `Tasks::run_until` with a `Cancellation` and a deadline.
- For channel operations, use `receive_until` and `send_until`.
- For a single drop-safe future (such as a receive, `Reply::closed`, `Watcher::wait_for`, or a socket read that loses nothing on drop), use `until` with a `Cancellation` and a deadline. Only race or pass to `until` futures that lose nothing when dropped.

## Limits

The rules have limits.

- **`select!` is still allowed.** `tokio::select!` and `futures::select!` are not banned yet, because Clippy ignores scoped allowances for some macro expansions. The ban waits for [the upstream fix](https://github.com/rust-lang/rust-clippy/pull/17862) to reach stable.
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
