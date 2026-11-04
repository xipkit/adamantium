# Adamantium

Adamantium is a fragment of Rust and the tools that keep a crate inside it. In the fragment, every value can cross a task. No lock outlives one synchronous method. No task outlives its scope. The only panics left are arithmetic overflow and stack exhaustion. Rust lets you write code like that. Adamantium makes it the only code you can write.

It has two halves. The `adamantium` library gives the fragment its vocabulary: copy-on-write values, a task scope, actors, and a bounded channel. The `cargo adamantium` subcommand runs Clippy with the rules that forbid everything else. The same subcommand also carries a [style review](#style-review) of the Google Rust style guide, built on the OpenAI Decisions API.

## Install

Add the library to a project:

```toml
[dependencies]
adamantium = { git = "https://github.com/xipkit/adamantium.git", rev = "..." }
```

Install the subcommand:

```sh
cargo install --locked --git https://github.com/xipkit/adamantium cargo-adamantium
```

The crate needs Rust 1.85 or later. Both halves follow their own rules, so the repository is also an example of the fragment.

## Check a project

```sh
cargo adamantium check              # run Clippy with the rules
cargo adamantium check --format json
cargo adamantium init               # write the rules into Cargo.toml and clippy.toml
cargo adamantium rules              # list the rules and why each exists
```

`check` runs `cargo clippy --all-targets` with adamantium's rules and its `clippy.toml`, and nothing else. Clippy's default lints are off for the run, so only the rules count. The rules run as warnings during the check, so a crate with findings still builds and the crates and tests that depend on it are checked too. Every workspace member is checked unless the arguments after `--` pick packages. It prints Clippy's diagnostics and a summary on stderr. With `--format json` it prints a report on stdout instead, one entry per finding with the rule, message, file, line, and column. Arguments after `--` go to `cargo clippy`. The exit status is `0` when clean, `1` with findings, and `2` when the project did not build.

The rules also require `overflow-checks = true` in the release profile. Integer overflow then panics in release builds, as it does in debug builds.

`init` writes the same rules into a project, so plain `cargo clippy` applies them. It writes the `[lints]` tables, or `[workspace.lints]` in a workspace, the release profile's overflow check, and a `clippy.toml`. Running it again changes nothing. It refuses to replace a `clippy.toml` that differs unless you pass `--force`.

An agent that writes code for the project can read the rules the same way. `cargo adamantium rules --format json` gives the full list and the reason behind each rule.

## The library

```rust
use adamantium::{Actor, Array, Channel, Map, Str, Tasks, blocking, concurrently};

// Values: a clone shares storage, and the first write copies it.
let items = Array::from([Str::from("a"), Str::from("b")]);
let mut more = items.clone();
more.push("c".into());
assert_eq!(items.len(), 2);

// A scope owns its tasks. It waits for all of them, and an error in any ends it.
let (profile, feed) = Tasks::run(async |t| {
    let profile = t.spawn(fetch("/profile"));
    let feed = t.spawn(fetch("/feed"));
    Ok((t.value(profile).await?, t.value(feed).await?))
})
.await?;

// Ordered parallel map, at most eight at a time.
let pages = concurrently(urls, 8, fetch).await?;

// An actor's methods run one at a time and never hold the lock across an await.
let tally = Actor::new(Tally::default());
tally.with(|t| t.record(&profile));
let total = tally.read(Tally::total);

// A bounded channel any task can send on or receive from.
let feed = Channel::<Str>::bounded(16);

// Code that blocks a thread runs on the blocking pool.
let text = blocking(|| std::fs::read_to_string("notes.txt")).await?;
```

- **`Str`, `Bytes`, `Array<T>`, and `Map<K, V>` are values.** Two names never see each other's writes. The runtime copies only when a shared value is written. Reads go through `Deref` where a slice or `str` view exists. `Map` has no `Index`, because indexing a missing key would panic. Writes that take a position return `None` or hand the value back instead of panicking. All four are `Send + Sync` when their element types are, and serialize with the `serde` feature.
- **`Tasks<E>` is a scope.** `spawn` starts a task and `value` waits for one. `join`, or the end of `Tasks::run`, waits for all of them. A task's error reaches the body through `value`. An error nobody asked for is reported at join, so it is never lost, and the first error aborts the rest. A panic in a task is resumed in the scope. Name the error type, `Tasks::<E>::run`, when nothing in the body fixes it, which is the case when every error passes through `?`.
- **`concurrently` is an ordered parallel map.** It runs a closure over items with a concurrency limit and returns the results in input order.
- **`blocking` is the one way to block a thread.** It runs a closure on the blocking pool and resumes its panic if it has one.
- **`Actor<S>` is state that tasks share.** `with` runs a closure with exclusive access, and `read` with shared access. A reading method such as `Tally::total` can be passed to `read` by name. Both are synchronous, so the lock cannot be held across an await. A `with` or `read` inside another, on any actor, panics instead of risking a deadlock.
- **`Channel<T>` is a bounded queue.** Every clone can send and receive. A producer calls `close` when it is done. Receivers then drain what remains and get `None`.

## The rules

The lint levels live in this repository's `[workspace.lints]` and the Clippy configuration in `clippy.toml`. `cargo adamantium rules` prints both. Together with the library they give a crate these properties:

| Property | Enforced by |
| --- | --- |
| No `unsafe`, so every other row rests on safe Rust | `unsafe_code = "forbid"` |
| Every value can cross a task | `future_not_send`; `Rc`, `RefCell`, `Cell`, and `LocalSet` disallowed |
| No task outlives its scope | `tokio::spawn`, `spawn_local`, `JoinSet`, `std::thread::spawn`, and `scope` disallowed; `Tasks` is the only way to start one |
| No lock is held across an await or inside another lock | `await_holding_lock`; `Mutex` and `RwLock` from std, tokio, parking_lot, and futures disallowed; `Actor::with` is synchronous and rejects nesting |
| No worker thread is blocked | `std::thread::sleep`, `spawn_blocking`, and `block_in_place` disallowed; `blocking` is the one way to block |
| No panic from indexing, unwrapping, or an explicit `panic!` | `indexing_slicing`, `string_slice`, `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable` |
| No silent truncation or wrap | `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`; `overflow-checks = true` in release |
| No leaks or cycles | `mem_forget`, `Box::leak`, `Arc::downgrade`, and `Weak` disallowed |
| No skipped cleanup | `exit` |

Tests are exempt from the panic rules. `unwrap`, indexing, and `panic!` are fine in `#[test]` functions and `#[cfg(test)]` modules. A helper function in an integration test file is not a test to Clippy, so it follows the rules.

The rules cannot promise everything. A receive that nobody sends to still hangs its scope, because no lint sees liveness. A dependency keeps its own unsafe code and panics. A Rust call that blocks without going through `blocking` still blocks a worker, because Clippy cannot tell what a foreign function does. Arithmetic overflow and stack exhaustion stay panics by choice. Checked arithmetic would put a `?` on every plus sign.

## Style review

`cargo adamantium style check` reviews the judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html) with the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions). It produces findings for an agent or a person to look into. It does not edit source, and it does not replace rustfmt, Clippy, or code review.

```sh
cargo adamantium style check                      # every .rs file under the current directory
cargo adamantium style check src/lib.rs --context "application code"
cargo adamantium style check . --rule unnecessary_deep_clone --dry-run
cargo adamantium style rules --format text
```

It prints JSON by default, or a readable report with `--format text`. It finds Rust files recursively, respects ignore files, and skips hidden directories, `target`, and `node_modules`. It does not resolve Cargo workspace membership. Each file goes to the API in one request, together with the selected rules and any context. A finding covers the whole file, because Decisions gives no trustworthy location. The agent inspects the file and makes its own edit. A file or context over the API's 256 KiB input limit is reported as an error, not truncated. The exit status is `0` for a clean check or a dry run, `1` for findings, and `2` for an incomplete check.

Transient failures are retried with exponential backoff and jitter, up to four attempts within three minutes per file. `Retry-After` is honored up to 30 seconds. Authentication, quota, and spending-limit errors are not retried. A service failure stops further requests and marks the remaining files as not checked. `--retry-from previous.json` resends only those files, with the same root, rules, and context, into a new report. Never redirect the output over the report being read.

Set `OPENAI_API_KEY`. On macOS the `openai-api-key` Keychain item is read when the variable is unset. Source files and context go to OpenAI. Results are model estimates, not deterministic lints, so evaluate them on labeled examples before letting them block anything. Source comments and strings are treated as untrusted evidence, and the question instructions say so. That does not guarantee resistance to adversarial input.

The rules live in [`style/rules/google-rust-decisions.json`](style/rules/google-rust-decisions.json): 51 questions, mapped to the guide in [`style/rules/coverage.md`](style/rules/coverage.md), with 114 labeled examples in [`style/fixtures/google-rust-cases.json`](style/fixtures/google-rust-cases.json). `node style/scripts/evaluate-decisions.mjs` runs the examples against the API, also with `--dry-run` and `--rule`. The full run makes 114 billable requests.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
node --test style/scripts/evaluate-decisions.test.mjs
```

[GitHub Actions](.github/workflows/ci.yml) runs the same four commands on pull requests and pushes to `main`.

The repository applies its own rules to itself. The library must use a few things it forbids, such as the `JoinSet` inside `Tasks` and the `Mutex` inside `Actor`. Each carries an `allow` with a reason. The CLI tests run `cargo adamantium check` against two fixture crates. One breaks every rule once, and one lives inside the fragment. They need Clippy, and network access the first time.
