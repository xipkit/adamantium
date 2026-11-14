# Prototype validation, 2026-11-14

Validated on OTIS, macOS, aarch64, with Rust 1.98.1 for the normal check and
`nightly-2026-08-20` for Dylint. `cargo-dylint`, `dylint-link`, and
`dylint_linting` were pinned to 6.4.0. These are single-run wall times, not a
statistical performance claim. No normal check loads the experimental library.

## Check cost

The existing `streaming` example was the timed in-tree consumer:

```sh
cargo-adamantium check . --format json -- --locked -p adamantium --example streaming
cargo-adamantium check . --format json --dylint-library "$LIBRARY" \
  -- --locked -p adamantium --example streaming
```

Each mode used its own initially empty `CARGO_TARGET_DIR`. Cargo registry
sources, the lint library, and the Dylint driver were already cached; this
measures cold **consumer compilation**, not first-time tool installation.
Warm runs immediately reused their respective targets. All four runs exited
0, with no findings or errors.

| Mode | Cold | Warm | Target disk after both runs |
| --- | ---: | ---: | ---: |
| Ordinary Clippy check | 5.209 s | 0.234 s | 106.8 MiB |
| Clippy plus experimental check | 9.220 s | 0.353 s | 193.2 MiB |

`du -sk` measured allocated disk. An isolated release lint build with cached
registry sources took 17.20 s cold and 0.06 s warm and used 134.1 MiB including
its dependencies. The release lint library was about 109 KiB. The cached driver
used 6.9 MiB; the installed nightly and components used 1,083 MiB. These figures
exclude shared Cargo registry caches and the tool executables. Dylint generates
and caches its own driver, resolving additional dependencies on first use; the
lint workspace lockfile does not lock that generated driver's dependencies.

Dylint 6.0.1 failed its driver build because it assumed an upstream `master`
branch. Version 6.4.0 completed setup. Do not infer that arbitrary older Dylint
versions work with this pin.

## Consumer and regression evidence

- The diagnostic suite checks exactly 34 blocking and 9 nested-access locations,
  plus one feature-gated blocking location. Negative cases include deferred
  closures/futures, unrelated methods and traits, in-memory I/O, source-level
  allows, and sanctioned blocking callbacks. It covers aliases, UFCS, macros,
  async closures/blocks, callback argument evaluation, repeated-callback traversal,
  and constant arguments inside async bodies.
- The real CLI integration suite verifies cold/warm report equality, package,
  feature and target selection, skips/allows/exceptions, Clippy-only defaults,
  missing/corrupt libraries, unavailable Dylint, and compilation failures.
- Both passes completed on a source archive of **Kan commit
  `3a7deb097021bd3c1fef6d5ec5cf4b2747179805`**, with `--locked --bin kan`.
  It reported 9 existing standard-rule findings and no experimental findings.
  A temporary async `std::fs::read` probe in that archive added exactly one
  experimental finding at the expected line. The probe was removed; Kan's
  repository was not changed. This validates the checker on Kan, not its server
  behavior or actor adoption (that snapshot does not depend on Adamantium).
- That consumer initially exposed a panic when a tracing macro generated a
  constant argument. The reduced fixture reproduced it before the fix. Runtime
  context analysis now excludes compile-time bodies, while nested runtime
  closures remain independently checked. Kan and the fixture then passed.
- The `streaming` example actually ran: ordered results 0 through 5, completion
  order 0/2/1, then cancellation with outstanding tasks drained. The actor
  consumer executable also ran and printed `actor consumer: 3`.
- Stable workspace tests, Clippy with warnings denied, formatting, and the
  missing-library regression passed locally. A separate CI job runs the pinned
  experimental suite on Linux. Hosted CI results remain the PR's authority;
  these local measurements do not claim Linux or Windows validation.

The prototype remains opt-in. It catches useful local execution patterns, but
its callback and indirect-call limits are material; see [README.md](README.md).
