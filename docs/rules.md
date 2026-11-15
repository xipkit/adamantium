# The rules

The default lint levels are in this repository's `[workspace.lints]`, and the Clippy configuration is in [`cli/clippy.toml`](../cli/clippy.toml). The repository's Cargo configuration points Clippy at that same bundled file. `cargo adamantium rules` prints both. The optional strict additions are recorded separately in `[workspace.metadata.adamantium-strict.lints]` and printed with `rules --strict`; they do not apply to this repository itself. The rules and library provide the following properties for code that follows them:

| Property | Enforced by |
| --- | --- |
| No `unsafe` code | `unsafe_code = "forbid"` |
| Every value can cross a task | `future_not_send`; `Rc`, `RefCell`, `Cell`, and `LocalSet` disallowed, and so are `LocalRuntime`'s entry points and the methods that read or write a `thread_local!` value, since a task can move to another thread at any await |
| No task outlives its scope | `tokio::spawn`, `spawn_local`, `JoinSet`, the spawn methods of Tokio's `Handle`, `Runtime`, `LocalRuntime`, and `task::Builder`, async-std's task and builder spawns, `smol::spawn`, smol's executor spawns, `std::thread::spawn`, and `scope` disallowed; tasks use `Tasks` |
| No lock is held across an await or inside another lock | `await_holding_lock`; `Mutex` and `RwLock` from std, tokio, parking_lot, and futures disallowed; `Actor::with` is synchronous and rejects nesting |
| Unbounded channel constructors restricted | Unbounded channel constructors from std, Tokio, async-channel, flume, and crossbeam-channel disallowed; use `Channel` |
| No channel drops messages for a receiver that falls behind | Tokio's `broadcast` channel disallowed; send a clone to one `Channel` per receiver, or use `tokio::sync::watch` when only the latest value matters |
| No worker thread is blocked | `std::thread::sleep`, `block_in_place`, every Tokio `spawn_blocking`, `async_std::task::spawn_blocking`, `smol::unblock`, and `block_on` on a `Handle` or `LocalRuntime` disallowed; blocking work uses `blocking` |
| No silently discarded must-use results or futures | `unused_must_use`, `let_underscore_must_use`, `let_underscore_future`; intentional discards use an explicit `drop` |
| No panic from indexing, unwrapping, or an explicit `panic!` | `indexing_slicing`, `string_slice`, `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`; `Vec::remove`, `Vec::insert`, `Vec::swap_remove`, `slice::split_at`, and `slice::copy_from_slice` disallowed |
| No silent truncation or wrap | `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`; `overflow-checks = true` in release |
| Explicit leak APIs and weak references restricted | `mem_forget`, `Box::leak`, `ManuallyDrop`, `Arc::downgrade`, and `Weak` disallowed |
| No skipped cleanup | `exit`; `std::process::abort` disallowed |

## Allowed, and why

Tokio's `Semaphore` is allowed, and so is holding a permit across an await, because a permit guards no data. To cap how many items run at once, use `concurrently` or the streaming variants. Don't acquire a second permit while holding one: two tasks that do so in opposite orders deadlock.

Tests are exempt from the panic rules, so `unwrap`, indexing, and `panic!` are allowed in `#[test]` functions and `#[cfg(test)]` modules. Before Rust 1.100, Clippy does not treat helper functions in integration test files as tests, so those functions follow the rules. From Rust 1.100, they are exempt as well.

## Timeouts

The rules ban `tokio::time::timeout`, which drops the future mid-await on expiry. Use `Tasks::run_until` with `Cancellation` and a deadline for scoped work, or `receive_until` and `send_until` for channel operations.

## Limits

The rules have limits. `tokio::select!` and `futures::select!` are not banned yet because Clippy ignores scoped allowances for some macro expansions; the ban awaits [the upstream fix](https://github.com/rust-lang/rust-clippy/pull/17862) reaching stable. A plain `receive` or `send` can wait indefinitely while the channel stays open and empty or full; use `receive_until` or `send_until` to bound the wait with a `Cancellation` or a deadline. Dependencies can still use unsafe code or panic; [`cargo adamantium deps`](cli.md#dependencies) shows which use `unsafe` or call disallowed paths. Clippy cannot detect every call that blocks a worker thread. `Runtime::block_on` is allowed because `#[tokio::main]` and `#[tokio::test]` expand to it; called inside a runtime, it panics at run time instead of blocking. The default tier permits arithmetic that can panic; the strict tier lints potentially panicking arithmetic operators. Neither tier prevents stack exhaustion or panics inside called methods.

Atomics and `OnceLock` and `LazyLock` statics are allowed. An atomic holds no guard across an await and can't deadlock, so it suits a single counter or flag; Clippy can't see a check-then-act spread over two atomics, so state whose parts must agree belongs in one `Actor`. A `OnceLock` or `LazyLock` static is set once, but whatever in it can still change, such as an `Actor`, is outside every scope and shared by every task and every test in the binary. Prefer passing an `Actor` to the tasks that use it.

Strong reference cycles are not detected or collected. An `Actor` whose state owns a clone of itself, directly or through other actors, keeps every state in the cycle alive after the last external handle is dropped. Banning `Weak` and `Arc::downgrade` does not prevent strong cycles; keep actor ownership acyclic or explicitly remove cycle-forming handles before releasing the last external handle.
