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

The default `tokio` feature provides `Task`, `Tasks`, `concurrently`, and
`blocking`. Set `default-features = false` to use `Actor`, `Str`, `Bytes`,
`Array`, `Map`, and `Channel` without Tokio, including on WASI. The `serde`
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
cargo adamantium init               # write the rules into Cargo.toml and clippy.toml
cargo adamantium rules              # list the rules and why each exists
```

`check` runs `cargo clippy --all-targets` with Adamantium's rules and `clippy.toml`. It disables Clippy's default lints and runs the rules as warnings, allowing crates with findings and their dependents to be checked. It checks every workspace member unless arguments after `--` select packages; those arguments are passed to `cargo clippy`.

The command prints diagnostics and a summary to stderr. With `--format json`, it prints a report to stdout with the rule, message, file, line, and column for each finding. The exit status is `0` when clean, `1` for findings, and `2` when the project did not build.

The rules also require `overflow-checks = true` in the release profile. Integer overflow then panics in release builds, as it does in debug builds.

`init` adds the rules to a project so plain `cargo clippy` applies them. It writes the `[lints]` tables, or `[workspace.lints]` in a workspace, the release profile's overflow check, and a `clippy.toml`. It excludes rules listed under `allow` or `exceptions` in the [skip list](#skipping-rules-and-paths). Running it again makes no changes, and it will not replace a different `clippy.toml` without `--force`.

## Skipping rules and paths

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

For a machine-readable list of rules and their reasons, run `cargo adamantium rules --format json`.

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

- **`Str`, `Bytes`, `Array<T>`, and `Map<K, V>`** use copy-on-write storage, so changes to a clone do not affect the original. Reads use `Deref` where a slice or `str` view exists. `Map` has no `Index` implementation, and writes that take a position return `None` or return the value instead of panicking. These types are `Send + Sync` when their element types are and support serialization with the `serde` feature.
- **`Tasks<E>`** manages a scope of tasks. `spawn` starts a task, and `value` waits for its result. `join` and `Tasks::run` wait for remaining tasks. Unobserved task errors are returned at join; if the body or an unobserved task returns an error, the remaining tasks are aborted. Task panics resume in the scope. Specify the error type with `Tasks::<E>::run` when it cannot be inferred, such as when all errors pass through `?`.
- **`concurrently`** maps items in parallel with a concurrency limit and returns results in input order.
- **`blocking`** runs a closure on the blocking pool and resumes any panic on the calling task.
- **`Actor<S>`** shares state between tasks. `with` provides exclusive access and `read` provides read-only access through synchronous closures, so neither holds the lock across an await. A read-only method such as `Tally::total` can be passed to `read`. Calling `with` or `read` from inside either method on any actor panics to prevent deadlocks.
- **`Channel<T>`** is a bounded queue whose clones can send and receive. Consume it with `split()` to get separate clonable **`Sender<T>`** and **`Receiver<T>`** handles. Dropping the last sender closes the channel; receivers drain queued messages and then receive `None`. Dropping the last receiver wakes pending sends with `Closed(value)`. Any retained combined `Channel` clones still own both endpoints, so drop or split all of them before relying on endpoint-drop closure. Either endpoint or a combined handle can also call `close()` explicitly.

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

## The rules

The lint levels are in this repository's `[workspace.lints]`, and the Clippy configuration is in `clippy.toml`. `cargo adamantium rules` prints both. The rules and library provide the following properties for code that follows them:

| Property | Enforced by |
| --- | --- |
| No `unsafe` code | `unsafe_code = "forbid"` |
| Every value can cross a task | `future_not_send`; `Rc`, `RefCell`, `Cell`, and `LocalSet` disallowed |
| No task outlives its scope | `tokio::spawn`, `spawn_local`, `JoinSet`, `std::thread::spawn`, and `scope` disallowed; tasks use `Tasks` |
| No lock is held across an await or inside another lock | `await_holding_lock`; `Mutex` and `RwLock` from std, tokio, parking_lot, and futures disallowed; `Actor::with` is synchronous and rejects nesting |
| No worker thread is blocked | `std::thread::sleep`, `spawn_blocking`, and `block_in_place` disallowed; blocking work uses `blocking` |
| No panic from indexing, unwrapping, or an explicit `panic!` | `indexing_slicing`, `string_slice`, `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable` |
| No silent truncation or wrap | `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`; `overflow-checks = true` in release |
| No leaks or cycles | `mem_forget`, `Box::leak`, `Arc::downgrade`, and `Weak` disallowed |
| No skipped cleanup | `exit` |

Tests are exempt from the panic rules, so `unwrap`, indexing, and `panic!` are allowed in `#[test]` functions and `#[cfg(test)]` modules. Clippy does not treat helper functions in integration test files as tests, so those functions follow the rules.

The rules have limits. A receive can wait indefinitely if no message arrives and the channel remains open. Dependencies can still use unsafe code or panic, and Clippy cannot detect every call that blocks a worker thread. Arithmetic overflow and stack exhaustion can still panic.

## Style review

`cargo adamantium style check` uses the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions) to review the judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). It returns findings for review; it does not edit source or replace rustfmt, Clippy, or code review.

```sh
cargo adamantium style check                      # every .rs file under the current directory
cargo adamantium style check src/lib.rs --context "application code"
cargo adamantium style check . --rule unnecessary_deep_clone --dry-run
cargo adamantium style rules --format text
```

The command prints JSON by default, or a readable report with `--format text`. It finds Rust files recursively, respects ignore files, and skips hidden directories, `target`, and `node_modules`. It does not resolve Cargo workspace membership. Each file is sent to the API in a request with the selected rules and any context. Findings apply to the whole file because Decisions does not provide reliable locations. The file, path, and context must fit within the API's 256 KiB JSON input limit. The exit status is `0` for a clean check or dry run, `1` for findings, and `2` for an incomplete check.

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
