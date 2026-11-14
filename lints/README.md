# Experimental execution-context lints

This opt-in prototype checks operations that Clippy's path lists cannot express:
blocking work in async execution and actor access nested inside an actor callback.
Ordinary `cargo adamantium check`, `init`, and library builds do not install or
load it. Actor-cycle detection is outside its scope. Keep the runtime nesting guard.

## Setup

Use a rustup-managed Cargo. Check out a **reviewed full commit SHA** of this
repository before building the library; record that SHA alongside your consumer's
check command. Do not track a moving branch for the lint binary. The checkout,
`lints/Cargo.lock`, exact nightly, and exact tool versions together identify the
experiment. Only load a library you trust: Dylint loads native code.

From that checkout's root:

```sh
rustup toolchain install nightly-2026-08-20 --profile minimal \
  --component rustc-dev,llvm-tools-preview,rustfmt
cargo install --locked --version 6.4.0 cargo-dylint dylint-link
./lints/build.sh
cargo build --locked -p cargo-adamantium
```

The isolated `lints/` workspace requires `nightly-2026-08-20`; root builds keep
the project's toolchain. `build.sh` enters that workspace so Cargo reads its
linker configuration. The library is named
`libadamantium_lints@nightly-2026-08-20-<host>.dylib` on macOS or `.so` on Linux,
in `lints/target/release` (or `$CARGO_TARGET_DIR/release` when set).
Use the full filename, including `@` and the host triple:

```sh
cargo adamantium check /path/to/consumer \
  --dylint-library /absolute/path/to/libadamantium_lints@nightly-2026-08-20-aarch64-apple-darwin.dylib \
  --format json -- --locked -p my-package --lib --features my-feature
```

The command runs normal Clippy, then checks the same selection with Dylint's
nightly compiler. Both passes must complete. Missing/incompatible tooling,
an invalid library, or compilation failure produces exit **2** and JSON
`status: "error"`, even if the other pass found violations. Findings use exit
**1**, clean checks **0**, and the existing schema-version-2 report. Consumer
Dylint metadata is ignored; only the explicitly selected library is loaded,
and both expected lint registrations are checked before compilation.

Package, feature, target, and other Cargo arguments after `--` go to both passes.
The default is the workspace and all targets; explicit package/target selection
is preserved. Cargo may still compile a selected binary's library dependency.
Dylint uses a separate target directory under the consumer's target directory.
A project requiring a different compiler may not compile on this nightly; that
is an incomplete check, never a fallback to Clippy-only success.

Use the existing `skip`, `allow`, and `exceptions` metadata with the exact names:

- `adamantium_blocking_in_async`
- `adamantium_nested_actor_access`

These names are experimental, separate from the standard `rules` list. `init`
does not write them into Rust's lint tables. Source-level `#[allow(...)]` also
works when Dylint is loaded; other compilers may warn about the unknown lint.

## Coverage and limits

Matching uses resolved definition paths and receiver types, not source spelling
or method-name matching. Renamed dependencies, imports, type aliases, function
item aliases, UFCS and macro expansions are covered. Diagnostics point to local
macro call sites. The fixtures assert exact diagnostic lines and absence of
additional diagnostics.

`adamantium_blocking_in_async` checks async functions, blocks, and closures. Its
explicit operation list is in [`src/lib.rs`](src/lib.rs): filesystem read/write,
copy, rename, removal, directory creation/enumeration, metadata, links and
permissions; `File` and `OpenOptions` operations; filesystem-querying `Path`
methods; TCP/UDP connect/bind/accept/send/receive and concrete file/socket
`Read`/`Write` operations; process status/output/wait; thread sleep/park/join.
It does not reject every function in those modules or every `Read` implementation:
an in-memory `Cursor` is allowed. It does not track socket nonblocking mode.

`adamantium_nested_actor_access` checks `Actor::with`, `read`, and `snapshot`
inside either access callback, on the same actor or a different actor. A closure
merely created inside a callback does not inherit its actor context; calling that
closure inline does. A future returned for later polling starts a fresh actor
context. Its body still gets the async blocking check.

Inline closure calls (including locally aliased closures), actor callbacks,
`Option`/`Result` mapping and fallback callbacks, and the listed eager `Iterator`
callbacks inherit execution context. Lazy iterator adapters are not followed.
`blocking` and `Tasks::spawn_blocking` callbacks start outside the caller's async
and actor context. Their **argument expressions** still execute on the caller.
An async body created inside a blocking callback is async code when later polled.
The visitor memoizes each body/context pair to avoid expanding repeated closure
calls exponentially.

This is local, conservative analysis, not a proof of nonblocking or deadlock-free
execution. It does not follow arbitrary helper functions, callbacks passed through
unknown APIs, function-pointer coercions, dynamic dispatch, custom I/O wrappers,
indirect dependency internals, or lazy iterator pipelines. It does not reason
about reachability, conditional execution, or which thread later polls a future.
Directly invoking a known blocking function as a higher-order function item
callback, rather than inside a closure, is also outside the prototype's coverage.
Unknown synchronous callback APIs can hide violations. Keep the runtime actor
checks; the lint cannot establish absence of indirect nesting or ownership cycles.

## Validation

After setup, from the root, with the normal project toolchain selected:

```sh
RUSTUP_TOOLCHAIN=stable python3 lints/test.py \
  --cli /absolute/path/to/cargo-adamantium \
  --library /absolute/path/to/libadamantium_lints@nightly-2026-08-20-<host>.so
```

On macOS use `.dylib`. The script copies fixtures into a temporary directory,
checks cold and warm diagnostics, package/feature/target selection, every skip
mechanism, ordinary Clippy-only behavior, and missing/corrupt tooling and failed
compilation. It includes a repeated-callback stress case. CI runs it separately
from the ordinary stable workspace checks. Windows has not been validated.

The [prototype measurements](MEASUREMENTS.md) record the tested consumer,
wall time, disk cost, and setup limitations. This is an opt-in research result;
those measurements do not establish compatibility with all consumers.
