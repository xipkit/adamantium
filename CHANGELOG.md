# Changelog

All notable changes to `adamantium` and `cargo-adamantium` are documented here.
Both crates share the workspace version.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `cargo-adamantium`: `style check --pack concurrency` reviews files for
  state with a joint invariant split across separate Actors, locks or
  atomics (`split_actor_invariant`) and unbounded in-memory collection growth
  (`unbounded_collection_growth`), and `style rules --pack concurrency` lists
  the pack's rules. It is not a blocking check yet.
- `cargo-adamantium`: the experimental lints follow local helper functions,
  including recursive ones, and report blocking, nested actor access and the
  new `adamantium_nested_limit` where async code or a callback calls the
  helper or awaits its future.
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
  or bounded depth-first search and replay saved failure schedules as a corpus.
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
- `cargo-adamantium`: `check --format json` and `check --format sarif` include
  an optional `remediation` for each finding, combining the replacement for its
  rule or disallowed path with the migration guide section or `cargo adamantium
  explain` command.

### Changed

- `cargo-adamantium`: a style finding's `guidance` says what goes wrong
  instead of repeating the question asked of the model. Reports record their
  rule pack and use schema version 4, so `--cache-from` needs a report from
  this version.
- `adamantium`: replay records use format 3 to capture the scheduler and its
  bounds. Regenerate saved records from previous formats.
- `cargo-adamantium`: Default rules report Tokio `oneshot`, Tokio `watch`,
  and `std::sync::mpsc::sync_channel`. Strict rules also report bounded Tokio
  `mpsc`, `Notify`, and `Semaphore`. Broadcast diagnostics recommend
  Adamantium's `Watch` when only the latest value matters.
- `cargo-adamantium`: Default rules report thread-local and implicitly
  seeded randomness from `rand` and `fastrand` and recommend
  `adamantium::random` for replayable draws.
- `cargo-adamantium`: The messages for Tokio's `broadcast` channel suggest
  `Fanout` alongside `Topic`.

### Fixed

- `adamantium`: Repeated uncontended calls to `Limit` or `KeyedLimit` now
  give other runnable tasks a turn when the cooperative budget runs out.
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

[Unreleased]: https://github.com/xipkit/adamantium/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/xipkit/adamantium/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/xipkit/adamantium/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/xipkit/adamantium/compare/286dc4203db35925e980a90d3dbb9c1f34f4e00c...v0.1.1
[0.1.0]: https://github.com/xipkit/adamantium/tree/286dc4203db35925e980a90d3dbb9c1f34f4e00c
