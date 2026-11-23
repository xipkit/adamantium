<img src="assets/adamantium-mark.svg" alt="Adamantium logo" width="128" height="128">

# Adamantium

Adamantium is a Rust library and a set of lint rules for concurrent code on Tokio.

- The library provides copy-on-write values, scoped tasks, actors, and bounded channels.
- The rules forbid the patterns that cause most concurrency bugs: detached tasks, locks held across an `await`, unbounded channels, and panics.
- The `cargo adamantium` command checks a project against the rules.

## Install

```sh
cargo add adamantium                      # the library
cargo install --locked cargo-adamantium   # the command
```

Adamantium requires Rust 1.96 or later.

See the [changelog](https://github.com/xipkit/adamantium/blob/main/CHANGELOG.md)
for changes to both crates between releases.

## Use the library

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

| Type | Purpose |
| --- | --- |
| `Str`, `Bytes`, `Array`, `Deque`, `Map`, `Set` | Copy-on-write values. A clone shares storage until one side writes. |
| `Tasks` | A scope that owns its tasks. It waits for them. If one fails, it aborts the others. `spawn_handled` sends a task's error to a handler instead, and `spawn_supervised` restarts a task that fails, within a `Restarts` budget. |
| `concurrently`, `first_ok` | Run a function on many items with a concurrency limit. |
| `Actor` | Shared state. Each access is a synchronous closure, so no lock is held across an `await`. |
| `BoundedMap` | A map that holds at most a fixed number of entries, evicts the least recently used one, and optionally expires entries by time to live. Use it for caches and session tables inside an `Actor`. |
| `AsyncActor` | One worker that owns its state and handles one message at a time. Use it when the state must `await`. |
| `Channel`, `Reply`, `Watch` | A bounded queue with single and batch receive, a one-shot reply, and the latest value of some state. |
| `Signal` | A wakeup for a worker. A notification sent while one is pending merges into it. |
| `Topic` | Sends each message to every subscriber. It waits for a slow subscriber and drops no message. |
| `Fanout` | Sends each event to every subscriber without waiting. It disconnects a subscriber that falls behind, which can then resubscribe. |
| `race` | Waits for the first of several futures and drops the others. Use it in place of `select!`. |
| `Cancellation` | A shutdown signal. `Tasks::run_until` and the `_until` methods stop on the signal or at a deadline. `Tasks::run_with_grace` lets admitted work finish first. |
| `retry`, `RateLimit`, `Limit`, `KeyedLimit` | Retry with backoff, cap the call rate, give each call one or more places in a concurrency budget, or serialize calls for the same key. |
| `SingleFlight` | Lets concurrent calls for the same key share one operation and its output. |
| `Once` | A value that async work makes once and then keeps, such as a connection. Concurrent first calls share one initializer. |
| `blocking` | Runs blocking code on the blocking pool. |
| `random` | Random draws that the deterministic harness records and replays. |

The [library guide](docs/library.md) describes each type, and [`examples/server.rs`](examples/server.rs) puts them together in a TCP server.

### Features

| Feature | Default | Adds |
| --- | --- | --- |
| `tokio` | Yes | `Tasks`, `concurrently`, `first_ok`, the concurrent streams, `AsyncActor`, `blocking`, `time`, `retry`, `Restarts`, and `RateLimit` |
| `serde` | No | Serialization for the values |
| `signal` | No | `Cancellation::cancel_on_signal` for SIGINT, SIGTERM, and Ctrl-C |
| `tracing` | No | Named task spans, and a warning that lists unfinished tasks when a deadline expires |
| `deterministic` | No | A test harness with random, PCT, and DFS scheduling, replay timelines, modeled duplex streams, and fault injection. See [Deterministic tests](docs/testing.md). |

With `default-features = false`, the values, `Actor`, `BoundedMap`, `Channel`, `Topic`, `Fanout`, `Reply`, `Watch`, `Signal`, `Limit`, `KeyedLimit`, `SingleFlight`, `Once`, and `race` work without Tokio.

### WASI

The library builds on `wasm32-wasip1` with and without the `tokio` feature. CI tests both. WASI has one thread and no panic unwinding, so these differences apply:

- `blocking` and `Tasks::spawn_blocking` run the closure on the one thread. Other tasks wait until it returns.
- A panic stops the program. On other targets, a scope reports the panic.
- The `signal` and `deterministic` features do not build.

## Check a project

```sh
cargo adamantium check          # run Clippy with the rules
cargo adamantium fix --dry-run  # show the automatic fixes as a diff
cargo adamantium fix            # apply the fixes, then check again
cargo adamantium init           # write the rules into Cargo.toml and clippy.toml
cargo adamantium rules          # list the rules
cargo adamantium explain unwrap_used   # show why a rule exists and how to fix a finding
cargo adamantium deps           # show which dependencies use unsafe code or spawn tasks
```

- `check` runs Clippy with the Adamantium rules on every workspace member. It prints text, JSON, or SARIF. It exits with `1` for findings or when `--fail-on-stale-baseline` detects a stale baseline, and `2` if the project did not build or the command failed.
- `fix` applies the rewrites that are safe to automate. It reports the others with a link to the section of the [migration guide](docs/migrating.md) that covers them, or, for a rule the guide doesn't cover, the `cargo adamantium explain` command for it. If the fixed project does not build, `fix` restores every file that it changed.
- `init` adds the rules to the project. After that, `cargo clippy` applies them without the command.

The [command reference](docs/cli.md) describes each subcommand, the report formats, and the CI setup.

## Adopt the rules in an existing project

Most findings in an existing async crate are `Arc<Mutex<T>>` and `tokio::spawn`. The [migration guide](docs/migrating.md) shows how to replace them with `Actor` and `Tasks`. You do not have to fix everything first. There are two ways to adopt the rules in steps.

A skip list in `Cargo.toml` turns off rules by path or by name:

```toml
[workspace.metadata.adamantium]
skip = ["tests/", "examples/"]              # ignore findings in these paths
allow = ["indexing_slicing", "unwrap_used"] # ignore these rules everywhere

[workspace.metadata.adamantium.exceptions]
future_not_send = ["src/syntax/"]           # ignore this rule in these paths
```

A baseline accepts the findings that exist today and fails only on new ones:

```sh
cargo adamantium check --format json > adamantium-baseline.json
cargo adamantium check --baseline adamantium-baseline.json
```

Add `--update-baseline` after fixing findings, or use `--fail-on-stale-baseline`
in CI to require that the baseline stays current. Add `--summary` to see
counts by rule and the files with the most findings.

## GitHub Action

After checkout and installing Rust with Clippy, use `xipkit/adamantium` with an immutable `rev` (or an exact crates.io `version`). See the [action setup](docs/cli.md#github-action).

| Input | Purpose |
| --- | --- |
| `sarif` | Set to `true` to write the check report to `runner.temp/adamantium.sarif`. Findings still fail the check. |
| `upload-sarif` | Set to `true` with `sarif: 'true'` to upload to GitHub code scanning; grant `security-events: write`. |
| `sarif-category` | Code scanning category, default `adamantium`; choose a distinct category for each analysis in a workflow. |
| `deep` | Set to `true` with a published CLI `version` to run crate-wide Dylint checks on Linux x86-64 (GNU) or macOS arm64. |
| `deps` | Set to `true` to run `deps --deny-new`. Without a baseline it checks against zero findings. |
| `deps-baseline` | Optional dependency report path, relative to `working-directory`; requires `deps: 'true'`. |
| `deps-args` | Cargo metadata/tree arguments for the audit, one per line. Set feature flags to match `cargo-args`; they are not copied automatically. Requires `deps: 'true'`. Use the same flags when creating the baseline. |
| `style` | Set to `google` or `concurrency` for an advisory style review. It sends source to OpenAI and requires `OPENAI_API_KEY` in the step environment; requests are billable. |
| `style-changed` | Optional Git base for the style review. Fetch enough history for the base and merge base to be available. |

Style findings and incomplete reviews produce a warning rather than failing the action; inspect the output. See [style review](docs/style-review.md) for its limitations and privacy details. To roll back these optional checks, remove their inputs from the workflow.

## The rules

Code that follows the rules has these properties:

| Property | How the rules enforce it |
| --- | --- |
| No `unsafe` code | `unsafe_code` is forbidden. |
| Every value can cross a task | Futures must be `Send`. `Rc`, `RefCell`, `Cell`, and thread-local values are not allowed. |
| No task outlives its scope | `tokio::spawn`, `JoinSet`, and `std::thread::spawn` are not allowed. Use `Tasks`. |
| No lock is held across an `await` or inside another lock | `Mutex` and `RwLock` are not allowed. Use `Actor`. |
| No queue grows without a limit | Unbounded channels are not allowed. Use `Channel`. For a cache or table inside an `Actor`, use `BoundedMap`. |
| No channel drops messages | Tokio's `broadcast` channel is not allowed. Use `Topic`, or `Fanout` to disconnect a subscriber that falls behind. |
| No worker thread is blocked | `std::thread::sleep`, `block_in_place`, and `spawn_blocking` are not allowed. Use `blocking`. |
| No timeout drops a future in the middle of its work | `tokio::time::timeout` and `timeout_at` are not allowed. Use `Tasks::run_until` or the `_until` methods. |
| Unstructured selection restricted | The rules report `futures::select!` and, on Tokio 1.44 or later, `tokio::select!` invocations with future branches. Use `first_ok`, `Tasks::run_until` with `Cancellation`, or `receive_until` and `send_until` for channel operations. |
| No result or future is discarded by accident | A discard must be an explicit `drop`. |
| No panic from indexing, `unwrap`, or `panic!` | These are not allowed outside tests. |
| No silent integer truncation or overflow | Lossy casts are not allowed. Release builds check for overflow. |
| No leak through an API | `mem::forget`, `Box::leak`, and `Weak` are not allowed. |
| No skipped cleanup | `process::exit` and `process::abort` are not allowed. |

The rules have limits. They stop at the crate boundary, and Clippy cannot detect every blocking call. See [The rules](docs/rules.md) for the exact lints, the exceptions, and the limits.

## Style review

`cargo adamantium style check` reviews Rust files against the judgment-based parts of the Google Rust style guide. It sends each file to the OpenAI Decisions API, so each request is billable and needs `OPENAI_API_KEY`. See [Style review](docs/style-review.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development commands and the release process.
