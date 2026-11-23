# Changelog

All notable changes to `adamantium` and `cargo-adamantium` are documented here.
Both crates share the workspace version.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `adamantium::Channel` and `Receiver` have `receive_batch_until`, whose linger
  starts with the first value, and `try_receive_batch` for taking ready values
  without waiting.
- `adamantium::testing::Replay::steps` counts the scheduling steps in a
  recorded execution, which shows how large a scenario is.
- `adamantium`: `Limit::weighted(places)` returns a `Weighted` handle whose
  `run` and `run_until` take several places at once, for a budget whose calls
  cost different amounts, such as bytes in flight. It replaces Tokio's
  `acquire_many`. A weight above the limit's total returns `Overweight`
  without waiting.

### Changed

- `adamantium`: `Limit` and `KeyedLimit` serve waiting calls in the order they
  started waiting, so small calls can't starve a large weighted one. A call
  that arrives while others wait queues behind them, even when places are
  free.
- Adamantium's deterministic timelines show the operation at modeled scheduling
  points, name parallel workers, and group repeated steps.
- `adamantium::testing::check` with `SchedulerMode::Dfs` fails with
  `FailureKind::IterationLimit` if schedules remain after `iterations`
  executions. It used to pass and report the shortfall only in
  `Replay::dfs_status`. `SchedulerMode::DfsPrefix` runs the same schedules and
  passes at the limit. Saved replay records move to format 5.
- The deterministic testing guide, `docs/testing.md`, now covers application
  code: shared state built from `Actor` and `Watch`, a protocol over modeled
  streams with faults and deadlines, wall-clock expiry, seeds in CI, seams for
  code the harness can't run, and choosing a scheduler. Its examples run as
  doctests.
- `cargo-adamantium fix` rewrites a `rand` draw to `adamantium::random` only
  when the draw is the argument of a standard `Duration` constructor, such as
  retry jitter, since it can't tell whether another draw is a key or a token.
  It names each draw it leaves, with the cryptographic randomness guidance.
  A draw in a `Duration` no longer needs a `u64` suffix or type argument.
- `cargo-adamantium`: Default rules report `tokio::time::timeout_at`, which
  drops its future when the deadline passes, as `tokio::time::timeout` does.

### Fixed

- Adamantium's deterministic failure reports show the environment settings
  and fixture steps for reproducing the failure through the harness.
- `cargo-adamantium` limits its replayable randomness recommendation to
  non-cryptographic draws and explains where to keep cryptographic randomness.
- `adamantium::testing::shrink` reduces failures whose task is still
  unwinding through modeled drops, such as a watch or an actor dropped after a
  failed assertion. It used to leave the test thread reporting
  `std::thread::panicking()`, and so did a `testing::replay` or
  `testing::replay_corpus` that departed from its record at such a point. Later
  harness calls in the same test then failed.
- Each `adamantium::testing` call runs on a new thread named like the test's,
  with the test's `tracing` subscriber and current span. An execution that
  failed while another task was still unwinding a panic, such as a step limit
  reached in a destructor or two workers that both panic, used to leave the
  test thread reporting `std::thread::panicking()`, and later harness calls in
  the same test failed. The test's other thread-locals are no longer visible to
  the scenario.

## [0.4.0] - 2026-11-21

### Added

- `cargo-adamantium check --deep` runs experimental crate-wide lints using
  Dylint for indirect blocking, nested actor and limit access, and races
  between timers and futures that may lose work when dropped. The published
  CLI pins the compiler and verifies the prebuilt library's checksum on
  Linux x86-64 (GNU) and macOS arm64.
- The composite GitHub Action installs and caches the CLI from an exact
  crates.io version or a full commit SHA. It can run deep checks, enforce a
  dependency audit baseline with Cargo feature selection, produce and upload
  SARIF with a configurable category, and run advisory style review.
- `adamantium::testing::DuplexStream` models a bounded byte stream with
  independent directions, partial reads and writes, virtual latency, and
  disconnects. Use the paired endpoints to test transport code through Tokio's
  `AsyncRead` and `AsyncWrite` traits.
- `adamantium::testing::explain` replays an execution and shows named tasks,
  virtual timestamps, waits, and wakeups. `testing::shrink` reduces a recorded
  schedule while reproducing the original failure.
- `adamantium::time::Interval` provides periodic ticks on the same clock as
  `time::now()`, with configurable missed-tick behavior and a cancellable
  `tick_until`.
- `adamantium`: `testing::FaultInjector`, `testing::FaultRule`, `testing::FaultStep`,
  and `testing::Latency` inject deterministic errors and virtual-clock latency into
  mocked async traits using `adamantium::random` and `adamantium::time::sleep`,
  reproducing identically under `testing::check` and `testing::replay`.
- `adamantium`: `time::wall_now()` reads the wall clock for comparing absolute
  timestamps such as a JWT or cookie `exp`. In the deterministic harness it
  starts at `testing::Config::wall_epoch` and advances with virtual time, and
  a `Replay` records the epoch, so expiry checks test and replay without real
  waits.
- `adamantium`: `SingleFlight::run_until` waits for a shared operation only
  until a `Cancellation` is requested or a deadline passes. A caller that
  stops leaves the operation to the others sharing it, and drops it only if
  it was the last.
- `adamantium`: `AsyncActor::new_named` names the actor worker for `tracing`
  spans, scope deadline reports, and `testing::explain` timelines.
  `AsyncActor::try_send` and `AsyncActor::send_until` match `Channel`, and
  `AsyncActor::call` and `AsyncActor::call_until` provide request-reply helpers
  backed by `Reply::pair()`.
- `adamantium`: `BoundedMap` holds at most a fixed number of entries and
  evicts the least recently used one to make room for a new key, so a cache
  or session table inside an `Actor` stays bounded. `insert` returns the
  evicted or replaced entry in `Inserted`.
- `cargo-adamantium`: `check --update-baseline` prunes resolved findings,
  `--fail-on-stale-baseline` requires an up-to-date baseline in CI, and
  `--summary` shows counts by rule and hotspot file.
- `cargo-adamantium` reads strict and baseline defaults for `check` and a
  dependency baseline for `deps --deny-new` from `Cargo.toml` metadata.
- `cargo-adamantium`: `fix` rewrites immediately awaited `sleep_until` calls,
  clock reads that supply a proved deadline, tracked `Arc<RwLock<T>>` state
  with direct-copy reads, and simple `u64` draws from `rand` and `fastrand`.
- `cargo-adamantium`: `style check --pack concurrency` reviews files for
  state with a joint invariant split across separate Actors, locks or
  atomics (`split_actor_invariant`) and unbounded in-memory collection growth
  (`unbounded_collection_growth`), and `style rules --pack concurrency` lists
  the pack's rules. It is not a blocking check yet.
- `cargo-adamantium`: the experimental lints follow local helper functions,
  including recursive ones, and report blocking, nested actor access and the
  new `adamantium_nested_limit` where async code or a callback calls the
  helper or awaits its future.
- `cargo-adamantium`: the experimental `adamantium_blocking_in_actor` lint
  reports file, network, process and thread-blocking operations inside an
  actor's `with` or `read` callback, directly or through local helpers, where
  they hold the actor's lock while they wait.
- `cargo-adamantium`: the experimental `adamantium_race_with_timer` lint
  accepts `Feed::receive` and `Watcher::changed` as drop-safe arms, since
  dropping either loses nothing.
- `adamantium`: `Tasks::spawner()` returns a cloneable `Spawner` so children
  can add siblings to their scope, with the same joining and error rules.
  Spawning after the scope closes returns `SpawnError` without starting work.
- `adamantium`: `Spawner` supports named, handled, and blocking spawns,
  including named handled and named blocking work.
- `adamantium`: `Fanout` sends each event to every subscribing `Feed`
  without waiting. A feed whose bounded queue is full is disconnected: it
  receives what it had queued, then `Ended::Overflowed`, and never a later
  event, so what a feed receives has no gaps. `Feed::resubscribe` starts a
  new feed to recover. Clones of a `Fanout` can all send, and every feed
  receives their sends in one order.
- `adamantium`: `Tasks::run_with_grace` shuts a scope down gracefully. A
  cancellation stops the body but lets admitted tasks finish for a grace
  period before the rest are aborted.
- `adamantium`: `KeyedLimit` serializes calls for the same key, including
  inside a connection `Limit`, and evicts keys after their last call ends.
- `adamantium`: deterministic tests can select probabilistic concurrency testing
  (PCT) or bounded depth-first search (DFS) and replay saved failure schedules
  as a regression corpus.
- `adamantium`: `random::u64` and `random::up_to` give application code random
  draws that the deterministic harness takes from its seed and a `Replay`
  repeats.
- `adamantium`: `Backoff::jitter` spreads each retry or restart wait between
  half of its delay and the whole of it, drawn from `random`. A backoff without
  it waits and schedules exactly as before.
- `adamantium`: `SingleFlight` lets concurrent calls for the same key share
  one operation and its output. Dropping the call that started the operation
  leaves it to the others, and dropping the last call drops it. It works
  without Tokio.
- `cargo-adamantium`: `deps --deny-new` lets CI reject increases in unsafe
  code or unscoped task starts across dependency updates using a JSON baseline.
  `deps` accepts Cargo feature flags so the audit can match the build.
- `adamantium`: `Watcher::changed` waits for a value the watcher hasn't
  seen and marks it seen without copying it, so the value needn't be `Clone`
  and no copy is made and dropped.
- `cargo-adamantium`: `fix` rewrites a local Tokio `oneshot` channel to
  `Reply::pair` and a local Tokio `watch` channel to `Watch::new` and
  `subscribe`, when every use of both halves reads the same afterwards. It
  leaves any other channel for review and says why, such as a watch send
  whose result is checked, which fails in Tokio once no receiver is alive.
- `cargo-adamantium`: `fix --strict` rewrites a local bounded Tokio `mpsc`
  channel with a positive literal capacity to `Channel::bounded(n).split()`,
  when its sender isn't cloned and each send is awaited where it is called,
  so that at most one send waits, as Tokio's admission order needs. Results
  of sends and of `try_recv` must be discarded or tested, and the guide's
  new section on bounded queues compares the two.
- In `adamantium`, `Bytes::slice` checks byte ranges and shares storage.
  Editing a window through `make_mut` copies only the selected bytes.
- In `adamantium`, `Str::slice` checks UTF-8 byte ranges and shares storage.
  Editing a window through `make_mut` copies only the selected text.
- `cargo-adamantium`: `check --format json` and `check --format sarif` include
  an optional `remediation` for each finding, combining the replacement for its
  rule or disallowed path with the migration guide section or `cargo adamantium
  explain` command.
- `adamantium`: `testing::Config::from_env()` applies `ADAMANTIUM_SEED` and
  `ADAMANTIUM_ITERATIONS` overrides to the deterministic testing defaults.

### Changed

- `adamantium`: deterministic test failures print the named task timeline
  alongside the replay JSON.
- `cargo-adamantium`: a style finding's `guidance` says what goes wrong
  instead of repeating the question asked of the model. Reports record their
  rule pack and use schema version 4, so `--cache-from` needs a report from
  this version.
- `adamantium`: replay records use format 4 to capture the scheduler, its
  bounds and the wall-clock epoch. Regenerate saved records from previous
  formats, including format 2 from 0.3.0.
- `adamantium`: `testing::Config` has a `wall_epoch` field. Struct literals
  that list every field need `..Config::default()`.
- `adamantium`: the `time` docs and the `tokio::time::Instant::now` lint
  message direct TTLs, cache expiry and retry windows to `time::now()`, and
  `std::time::Instant::now()` only to metrics and logging.
- `cargo-adamantium`: Default rules report `std::time::SystemTime::now` and
  recommend `adamantium::time::wall_now` for comparing absolute timestamps
  such as a JWT or cookie `exp`. A timestamp only for logging can keep it
  under an `#[expect]` with a reason.
- `cargo-adamantium`: Default rules report Tokio `oneshot`, Tokio `watch`,
  and `std::sync::mpsc::sync_channel`. Strict rules also report bounded Tokio
  `mpsc`, `Notify`, and `Semaphore`. Broadcast diagnostics recommend
  Adamantium's `Watch` when only the latest value matters.
- `cargo-adamantium`: Default rules report thread-local and implicitly
  seeded randomness from `rand` and `fastrand` and recommend
  `adamantium::random` for replayable draws.
- The default CLI rules report `futures::select!` and, with Tokio 1.44 or later,
  `tokio::select!` with future branches. Use scoped tasks or the cancellation
  operations for the work being selected.
- `cargo-adamantium`: The messages for Tokio's `broadcast` channel suggest
  `Fanout` alongside `Topic`.

### Fixed

- Ready async channel operations, ready interval ticks, and uncontended
  `Limit` or `KeyedLimit` calls give other runnable tasks a turn when the
  cooperative budget runs out.
- `Tasks` preserves the join notification for a completed task even when
  another task has already reaped its completion.
- `adamantium`: Dropping a `Channel`'s last receiver drops the values still
  queued, as Tokio's `mpsc` receivers do. Before, they lived until the last
  sender dropped too, so a queued `Reply` kept its requester waiting.
- `adamantium`: `time::sleep` caps waits longer than 30 years at 30 years on
  every supported Tokio version. Tokio before 1.54 kept them, so production
  could wait longer than the deterministic clock.

## [0.3.0] - 2026-11-17

### Fixed

- `cargo-adamantium`: `fix` links each finding it leaves to the migration
  guide's section by its rule and disallowed path. A finding the guide has no
  section on, such as `unwrap_used`, gets the `cargo adamantium explain`
  command for its rule instead of a link to the Tasks section.

## [0.2.0] - 2026-11-15

### Added

- `adamantium`: `Topic` and `Subscriptions` broadcast each message to every
  subscriber through bounded queues, waiting for slow subscribers instead of
  dropping messages.
- `adamantium`: `Limit` caps how many calls run at once across tasks. Its
  `run_until` method also supports cancellation and deadlines.
- `adamantium`: `race` waits for the first ready future in a tuple of two to
  eight futures and drops the others. It works without Tokio.
- `adamantium`: `Tasks::spawn_supervised` and `spawn_supervised_named` restart
  failed tasks within a `Restarts` budget, with optional backoff. Named
  supervised tasks support tracing spans and restart warnings.

### Changed

- `adamantium`: on `wasm32-wasip1`, `blocking` and `Tasks::spawn_blocking` run
  closures on the single thread instead of using a blocking pool. Other tasks
  wait until the closure returns, and a panic terminates the program.
- `cargo-adamantium`: broadcast lint diagnostics recommend `Topic` as the
  replacement for Tokio broadcast channels.
- Shortened the README and moved the library, CLI, rules, style review, and
  deterministic testing references into dedicated guides under `docs/`.

## [0.1.1] - 2026-11-14

### Changed

- Updated both crates' README with crates.io installation instructions and
  moved development and release instructions to `CONTRIBUTING.md`.
- Published both crates through crates.io Trusted Publishing for the first
  time. The library and CLI source are unchanged from 0.1.0.

## [0.1.0] - 2026-11-14

### Added

- Initial crates.io releases of `adamantium` and `cargo-adamantium`.
- `adamantium`: copy-on-write values (`Str`, `Bytes`, `Array`, `Deque`, `Map`,
  and `Set`), synchronous and asynchronous actors, bounded channels, one-shot
  replies, and watches.
- `adamantium`: scoped tasks, cancellation and deadlines, bounded concurrent
  streams, retries with backoff, rate limits, and blocking work through the
  default `tokio` feature. Optional features provide Serde support, tracing,
  process signal cancellation, and deterministic testing with replay.
- `cargo-adamantium`: `check`, `init`, `rules`, and `explain` for enforcing the
  lint rules; `fix` for mechanical migrations; `deps` for auditing dependencies;
  and `style` for reviewing code against the Google Rust style guide. Checks
  support text, JSON, SARIF, baselines, and an optional strict lint tier.

[Unreleased]: https://github.com/xipkit/adamantium/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/xipkit/adamantium/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/xipkit/adamantium/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/xipkit/adamantium/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/xipkit/adamantium/compare/286dc4203db35925e980a90d3dbb9c1f34f4e00c...v0.1.1
[0.1.0]: https://github.com/xipkit/adamantium/tree/286dc4203db35925e980a90d3dbb9c1f34f4e00c
