# Changelog

All notable changes to `adamantium` and `cargo-adamantium` are documented here.
Both crates share the workspace version.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `adamantium`: `Tasks::spawner()` returns a cloneable `Spawner` so children
  can add siblings to their scope, with the same joining and error rules.
  Spawning after the scope closes returns `SpawnError` without starting work.
- `adamantium`: `KeyedLimit` serializes calls for the same key, including
  inside a connection `Limit`, and evicts keys after their last call ends.
- `adamantium`: deterministic tests can select probabilistic concurrency testing
  or bounded depth-first search and replay saved failure schedules as a corpus.

### Changed

- `adamantium`: replay records use format 3 to capture the scheduler and its
  bounds. Regenerate saved records from previous formats.

### Changed

- `cargo-adamantium`: Default rules report Tokio `oneshot`, Tokio `watch`,
  and `std::sync::mpsc::sync_channel`. Strict rules also report bounded Tokio
  `mpsc`, `Notify`, and `Semaphore`. Broadcast diagnostics recommend
  Adamantium's `Watch` when only the latest value matters.

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
