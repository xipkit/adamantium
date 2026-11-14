<img src="assets/adamantium-mark.svg" alt="Adamantium logo" width="128" height="128">

# Adamantium

Adamantium is a Rust library and set of lint rules for writing concurrent code with copy-on-write values, scoped tasks, actors, and bounded channels. The rules restrict features such as detached tasks, locks held across `await`, and common sources of panics.

The `cargo adamantium` subcommand checks projects against these rules. It also offers a [style review](#style-review) based on the Google Rust style guide and the OpenAI Decisions API.

## Install

Add the library to a project:

```toml
[dependencies]
adamantium = { git = "https://github.com/xipkit/adamantium.git", rev = "..." }
```

The default `tokio` feature provides `Task`, `Tasks`, `concurrently`, the
concurrent streams, and `blocking`. Set `default-features = false` to use `Actor`, `Str`, `Bytes`,
`Array`, `Deque`, `Map`, `Set`, and `Channel` without Tokio, including on WASI. The `serde`
feature can be enabled independently.

Install the subcommand:

```sh
cargo install --locked --git https://github.com/xipkit/adamantium cargo-adamantium
```

Adamantium requires Rust 1.88 or later. This repository uses the library and follows the same rules.

## Check a project

```sh
cargo adamantium check              # run Clippy with the rules
cargo adamantium check --format json
cargo adamantium check --format sarif  # for GitHub code scanning
cargo adamantium init               # write the rules into Cargo.toml and clippy.toml
cargo adamantium rules              # list the rules and why each exists
cargo adamantium explain unwrap_used # show why a rule exists and how to fix it
cargo adamantium explain clippy::rc_buffer --format json
```

`check` runs `cargo clippy --all-targets` with Adamantium's rules and `clippy.toml`. It disables Clippy's default lints and runs the rules as warnings, allowing crates with findings and their dependents to be checked. It checks every workspace member unless arguments after `--` select packages; those arguments are passed to `cargo clippy`.

The command prints diagnostics and a summary to stderr. With `--format json`, it prints a report to stdout with the rule, message, file, line, column, and source for each finding. The source is the text of the line the finding starts on, with leading and trailing whitespace removed; it is `null`, like the line and column, for a finding without a line. The exit status is `0` when clean, `1` for findings, and `2` when the project did not build.

The JSON check report uses `schema_version: 3`. Its `warnings` array contains warnings about Adamantium's Clippy configuration and unknown, renamed, or removed lints, each with a message, file, and line. Warnings do not change the status or exit code. Configuration warnings and errors name the file `clippy.toml`; their lines refer to Adamantium's bundled configuration, which `cargo adamantium rules` prints. Lint warnings without a primary span (`E0602` and `renamed_and_removed_lints`) have `file: null` and `line: null` and appear once per code and message across targets. Compiler summaries are omitted. Other compiler warnings appear only in text output.

With `--format sarif`, it prints a [SARIF 2.1.0](https://docs.oasis-open.org/sarif/sarif/v2.1.0/sarif-v2.1.0.html) log to stdout, which GitHub code scanning shows as annotations on pull requests. Each finding is an `error` result for its rule. Build errors and warnings are tool notifications rather than results, so they raise no alerts. File URIs are relative to the root of the git repository holding the workspace, or to the workspace root outside git. A finding without a line, such as a missing `overflow-checks`, points at the first line of its file. The exit status is the same as for the other formats.

```yaml
permissions:
  security-events: write
steps:
  - uses: actions/checkout@v4
  - run: cargo install --locked --git https://github.com/xipkit/adamantium cargo-adamantium
  # Findings exit with 1; let them through so code scanning can show them.
  - run: cargo adamantium check --format sarif > adamantium.sarif || [ $? -eq 1 ]
  - uses: github/codeql-action/upload-sarif@v4
    if: always()
    with:
      sarif_file: adamantium.sarif
      category: adamantium
```

The rules also require `overflow-checks = true` in the release profile. Integer overflow then panics in release builds, as it does in debug builds.

`init` adds the rules to a project so plain `cargo clippy` applies them. It writes the `[lints]` tables, or `[workspace.lints]` in a workspace, the release profile's overflow check, and a `clippy.toml`. It excludes rules listed under `allow` or `exceptions` in the [skip list](#skipping-rules-and-paths). Running it again makes no changes, and it will not replace a different `clippy.toml` without `--force`.

### Strict tier

Use `--strict` to add stricter rules to the default set:

```sh
cargo adamantium check --strict
cargo adamantium init --strict
cargo adamantium rules --strict              # also supports --format json or toml
```

The strict tier starts with [`arithmetic_side_effects`](https://clippy.rust-lang.org/master/index.html#arithmetic_side_effects). It flags arithmetic operators that can overflow or panic, including integer division and negation. Use methods such as `checked_add`, `checked_div`, `saturating_add`, or `wrapping_add` to make the intended behavior explicit. Clippy recognizes some safe operations, but cannot prove every application's arithmetic bounds, so this tier is opt-in.

`init --strict` adds the strict lint levels to the project's tables so plain `cargo clippy` applies them. Keep passing `--strict` to `check` in CI to include those rules in Adamantium's reports. Repeating `init --strict` makes no changes; running `init` afterward preserves previously added strict lint levels. The same skip lists apply to both tiers.

## Skipping rules and paths

Most findings in an existing async crate are `Arc<Mutex<T>>` and `tokio::spawn`. The [migration guide](docs/migrating.md) shows how to rewrite each with `Actor` and `Tasks`.

To adopt the rules gradually, add a skip list to `Cargo.toml`:

```toml
[workspace.metadata.adamantium]
skip = ["tests/", "examples/"]              # skip findings in these paths
allow = ["indexing_slicing", "unwrap_used"] # skip these rules throughout the project

[workspace.metadata.adamantium.exceptions]
future_not_send = ["src/syntax/", "src/bytecode.rs"] # skip this rule in these paths
```

For a single package, use `[package.metadata.adamantium]`. Paths are globs relative to the directory containing `Cargo.toml`; paths without wildcards also cover their contents. Use either the bare rule name or the qualified name printed by `cargo adamantium rules`. Unknown rule names produce an error.

`check` summarizes skipped findings and reports counts by rule in the JSON report's `skipped` map. Remove a rule from `allow` to see its findings. Projects with a skip list should run `cargo adamantium check` in CI because a `[lints]` table cannot scope a rule to a path.

## Baselines

The skip list adopts rules by path or by rule. To adopt them everywhere at once without fixing every existing finding first, save a report and check against it:

```sh
cargo adamantium check --format json > adamantium-baseline.json
cargo adamantium check --baseline adamantium-baseline.json
```

With `--baseline`, findings that the earlier report already has are left out, so only new findings fail the check. The status, the exit code, and the findings in JSON and SARIF reports cover only new findings, and text output shows diagnostics only for them. The JSON report counts the matched findings by rule in its `baselined` map, and `baseline_unmatched` is the number of findings in the baseline that the check did not find, or `null` without a baseline. Regenerate the baseline after fixing findings so they cannot come back.

A finding matches the baseline when its rule, its path relative to the project root, and its source text are the same. Line numbers, columns, and messages are ignored, so edits elsewhere in a file and a different Clippy version do not bring findings back, and the baseline can be written on another machine. Editing the line of a finding, or moving it to another file, makes it new. When findings share a rule, path, and source text, only those beyond the baseline's count are new. The skip list applies first. The baseline must be a `schema_version: 3` report from a check that built; `check` refuses older reports and reports from failed builds.

For a machine-readable list of rules and their reasons, run `cargo adamantium rules --format json`.

`cargo adamantium explain <rule>` prints the rule's reasoning, recommended replacement, and before-and-after example without loading a project or accessing the network. It accepts the bare or qualified rule name, including `overflow_checks` or `adamantium::overflow_checks` for the release profile check. With `--format json`, it prints an object with `schema_version: 1`, `rule` (the qualified name), `why`, `replacement`, `before`, and `after`. Successful explanations exit with `0`; unknown rule names exit with `2` and an error on stderr.

## The library

```rust
use adamantium::{Actor, Array, Channel, Map, Str, Tasks, blocking, concurrently};

// Clones share storage until a write makes a copy.
let items = Array::from([Str::from("a"), Str::from("b")]);
let mut more = items.clone();
more.push("c".into());
assert_eq!(items.len(), 2);

// A scope owns its tasks and waits for them to finish.
let (profile, feed) = Tasks::run(async |t| {
    let profile = t.spawn(fetch("/profile"));
    let feed = t.spawn(fetch("/feed"));
    Ok((t.value(profile).await?, t.value(feed).await?))
})
.await?;

// Map in parallel, with a concurrency limit of eight.
let pages = concurrently(urls, 8, fetch).await?;

// Access shared state without holding a lock across an await.
let tally = Actor::new(Tally::default());
tally.with(|t| t.record(&profile));
let total = tally.read(Tally::total);

// Send and receive through a bounded channel.
let feed = Channel::<Str>::bounded(16);

// Run blocking work on the blocking pool.
let text = blocking(|| std::fs::read_to_string("notes.txt")).await?;
```

- **`Str`, `Bytes`, `Array<T>`, `Deque<T>`, `Map<K, V>`, and `Set<T>`** use copy-on-write storage, so changes to a clone do not affect the original. `Str`, `Bytes`, and `Array` use `Deref` for reads through `str` or slice views; `Deque` uses `Deref` to `VecDeque` and supports pushing and popping at both ends. `Map` has no `Index` implementation, and writes that take a position return `None` or return the value instead of panicking. These types are `Send + Sync` when their element types are and support serialization with the `serde` feature.
- **`Set<T>`** stores unique values in insertion order, removes values without reordering the rest, and compares membership regardless of order. Union, intersection, difference, and symmetric difference borrow values in the documented set order; collect an iterator to make an owned set. With `serde`, sets serialize as sequences in insertion order and deserialize duplicate values only once.
- **`Tasks<E>`** manages a scope of tasks. `spawn` starts a task, and `value` waits for its result. `join` and `Tasks::run` wait for remaining tasks. Unobserved task errors are returned at join; if the body or an unobserved task returns an error, the remaining tasks are aborted. Task panics resume in the scope. Specify the error type with `Tasks::<E>::run` when it cannot be inferred, such as when all errors pass through `?`.
- **`concurrently`** maps items in parallel with a concurrency limit and returns results in input order. It starts when first polled and takes each item only when a place opens, so at most `limit` futures exist at once and a failure stops it taking more. The iterator and closure must be `Send`, and they run inside the returned future's poll.
- **`blocking`** runs a closure on the blocking pool and resumes any panic on the calling task.

- **`Actor<S>`** shares state between tasks. `with` provides exclusive access and `read` provides read-only access through synchronous closures, so neither holds the lock across an await. A read-only method such as `Tally::total` can be passed to `read`. Calling `with` or `read` from inside either method on any actor panics to prevent deadlocks.
- **`Channel<T>`** is a bounded queue whose clones can send and receive. Consume it with `split()` to get separate clonable **`Sender<T>`** and **`Receiver<T>`** handles. Dropping the last sender closes the channel; receivers drain queued messages and then receive `None`. Dropping the last receiver wakes pending sends with `Closed(value)`. Any retained combined `Channel` clones still own both endpoints, so drop or split all of them before relying on endpoint-drop closure. Either endpoint or a combined handle can also call `close()` explicitly.

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

With the `tokio` feature, use `adamantium::time::sleep(duration)` and
`adamantium::time::sleep_until(deadline)` for waits. These functions and
`time::now()` currently use Tokio's clock, including its paused test clock.
Build periodic waits with a loop over `time::sleep_until`. For measuring elapsed
wall-clock time, `std::time::Instant::now()` remains allowed.

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

### Retries and rate limits

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

### Streaming concurrent maps

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

With `serde`, `Bytes` accepts numeric sequences in human-readable formats and
byte strings in binary formats; JSON strings are rejected. Binary byte strings
currently fail to deserialize when Serde buffers them for `#[serde(flatten)]`,
`#[serde(untagged)]`, internally tagged enums (`#[serde(tag = "...")]`), or
adjacently tagged enums (`#[serde(tag = "...", content = "...")]`) when the
content comes before the tag. Serde's buffer reports itself as human-readable,
so `Bytes` requests a sequence instead of a byte string. This hint preserves
support for text formats such as quick-xml, csv, and envy, whose
`deserialize_any` does not produce a sequence.

## Deterministic Actor tests

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

This first stage supports Actor only. `testing::parallel` provides independent
test workers; it does not model Tasks' cleanup semantics. Channel, Tasks,
Cancellation and blocking work reject use in the harness until their adapters
land (ADAM-159 and ADAM-160); virtual deadlines follow in ADAM-161. Use `#[test]`,
not `#[tokio::test]`, and create actors inside the execution that uses them.
Replace I/O, real time, uncontrolled randomness and foreign executors with
deterministic fixtures. Arbitrary foreign futures cannot all be diagnosed, and
a step bound cannot interrupt a loop that makes no scheduling calls.

## The rules

The default lint levels are in this repository's `[workspace.lints]`, and the Clippy configuration is in `clippy.toml`. `cargo adamantium rules` prints both. The optional strict additions are recorded separately in `[workspace.metadata.adamantium-strict.lints]` and printed with `rules --strict`; they do not apply to this repository itself. The rules and library provide the following properties for code that follows them:

| Property | Enforced by |
| --- | --- |
| No `unsafe` code | `unsafe_code = "forbid"` |
| Every value can cross a task | `future_not_send`; `Rc`, `RefCell`, `Cell`, and `LocalSet` disallowed, and so are `LocalRuntime`'s entry points |
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

Tokio's `Semaphore` is allowed, and so is holding a permit across an await, because a permit guards no data. To cap how many items run at once, use `concurrently` or the streaming variants. Don't acquire a second permit while holding one: two tasks that do so in opposite orders deadlock.

Tests are exempt from the panic rules, so `unwrap`, indexing, and `panic!` are allowed in `#[test]` functions and `#[cfg(test)]` modules. Before Rust 1.100, Clippy does not treat helper functions in integration test files as tests, so those functions follow the rules. From Rust 1.100, they are exempt as well.

The rules have limits. A plain `receive` or `send` can wait indefinitely while the channel stays open and empty or full; use `receive_until` or `send_until` to bound the wait with a `Cancellation` or a deadline. Dependencies can still use unsafe code or panic, and Clippy cannot detect every call that blocks a worker thread. `Runtime::block_on` is allowed because `#[tokio::main]` and `#[tokio::test]` expand to it; called inside a runtime, it panics at run time instead of blocking. The default tier permits arithmetic that can panic; the strict tier lints potentially panicking arithmetic operators. Neither tier prevents stack exhaustion or panics inside called methods.

Strong reference cycles are not detected or collected. An `Actor` whose state owns a clone of itself, directly or through other actors, keeps every state in the cycle alive after the last external handle is dropped. Banning `Weak` and `Arc::downgrade` does not prevent strong cycles; keep actor ownership acyclic or explicitly remove cycle-forming handles before releasing the last external handle.

## Style review

`cargo adamantium style check` uses the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions) to review the judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). It returns findings for review; it does not edit source or replace rustfmt, Clippy, or code review.

```sh
cargo adamantium style check                      # every .rs file under the current directory
cargo adamantium style check src/lib.rs --context "application code"
cargo adamantium style check . --rule unnecessary_deep_clone --dry-run
cargo adamantium style rules --format text
```

The command prints JSON by default, or a readable report with `--format text`. It finds Rust files recursively, respects ignore files, and skips hidden directories, `target`, and `node_modules`. It does not resolve Cargo workspace membership. Each file is sent to the provider in a request with the selected rules and any context. `--provider` selects the service that answers the rules; `decisions`, the OpenAI Decisions API, is the default and currently the only provider. Reports record the provider, and `--retry-from` refuses a report from a different one. Findings apply to the whole file because Decisions does not provide reliable locations. The file, path, and context must fit within the API's 256 KiB JSON input limit. The exit status is `0` for a clean check or dry run, `1` for findings, and `2` for an incomplete check.

Transient failures are retried with exponential backoff and jitter, up to four attempts within three minutes per file. `Retry-After` is honored up to 30 seconds. Authentication, quota, and spending-limit errors are not retried. A service failure stops further requests and marks the remaining files as unchecked. Use `--retry-from previous.json` to retry failed and unchecked files in a new report. Supply the same scan root and `--rule` options, and pass `--context` again if needed. Do not redirect the output over the report being read.

Set `OPENAI_API_KEY`. On macOS, the `openai-api-key` Keychain item is used when the variable is unset. Source files and context are sent to OpenAI. Results are model estimates. Evaluate them on labeled examples before using them as a blocking check. The questions instruct the model to treat source comments and strings as untrusted evidence, but this does not guarantee resistance to adversarial input.

The rules are in [`style/rules/google-rust-decisions.json`](style/rules/google-rust-decisions.json), with their guide coverage in [`style/rules/coverage.md`](style/rules/coverage.md) and labeled examples in [`style/fixtures/google-rust-cases.json`](style/fixtures/google-rust-cases.json). Run `node style/scripts/evaluate-decisions.mjs` to evaluate the examples against the API. It also accepts `--dry-run` and `--rule`. The full run makes 114 billable requests.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked -p adamantium --no-default-features
cargo clippy --locked -p adamantium --all-targets --no-default-features -- -D warnings
cargo test --locked -p adamantium --no-default-features
node --test style/scripts/evaluate-decisions.test.mjs
```

[GitHub Actions](.github/workflows/ci.yml) runs the same commands on pull requests and pushes to `main`.

The repository follows its own rules. Where the library needs restricted features, such as the `JoinSet` inside `Tasks` and the `Mutex` inside `Actor`, the code uses an `allow` with a reason. The CLI tests run `cargo adamantium check` against fixture crates with and without rule violations. They require Clippy and network access on the first run.
