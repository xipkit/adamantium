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

The default `tokio` feature provides `Task`, `Tasks`, `concurrently`, `first_ok`, the
concurrent streams, `AsyncActor`, and `blocking`. Set `default-features = false` to use `Actor`, `Str`, `Bytes`,
`Array`, `Deque`, `Map`, `Set`, `Channel`, `Reply`, and `Watch` without Tokio, including on WASI. The `serde`
feature can be enabled independently.

Install the subcommand:

```sh
cargo install --locked --git https://github.com/xipkit/adamantium cargo-adamantium
```

Adamantium requires Rust 1.96 or later. This repository uses the library and follows the same rules.

## Check a project

```sh
cargo adamantium check              # run Clippy with the rules
cargo adamantium check --format json
cargo adamantium check --format sarif  # for GitHub code scanning
cargo adamantium fix --dry-run      # preview mechanical fixes as a unified diff
cargo adamantium fix                # apply fixes, then check again
cargo adamantium init               # write the rules into Cargo.toml and clippy.toml
cargo adamantium rules              # list the rules and why each exists
cargo adamantium explain unwrap_used # show why a rule exists and how to fix it
cargo adamantium explain clippy::rc_buffer --format json
cargo adamantium deps               # show which dependencies use unsafe code or spawn tasks
```

`check` runs `cargo clippy --all-targets` with Adamantium's rules and `clippy.toml`. It disables Clippy's default lints and runs the rules as warnings, allowing crates with findings and their dependents to be checked. It checks every workspace member unless arguments after `--` select packages; those arguments are passed to `cargo clippy`.

`fix` uses the same Clippy pipeline and skip configuration. It applies `MachineApplicable` suggestions first, then rewrites immediately awaited calls that Clippy resolves to `tokio::time::sleep` and immediate `tokio::task::spawn_blocking(f).await.unwrap()` expressions (including imported aliases). Migration rewrites require a nonoptional, noninherited dependency named `adamantium`; the Tokio rewrites also need default features or an explicit `tokio` feature. Compound callees, stored sleep futures, and blocking calls with explicit type arguments are left for review. It leaves macro expansions, external source files, conflicting suggestions, and decisions about task scopes or lock lifetimes for review, with links to the [migration guide](docs/migrating.md).

`fix` also turns a local `Arc<Mutex<T>>` into an `Actor<T>` when every use of it, and of every clone of it, is one the guide marks as mechanical: the constructor, `Arc::clone`, a guard used within one statement (as `with`, or `read` when it only reads; a method called on the guard is called on `(*m)`, which keeps its method lookup), `lock().unwrap().clone()` (as a guard statement, such as `with(|m| (*m).clone())`; shorten it to `snapshot()` when the state itself is `Clone`), and `Arc::try_unwrap` or `Arc::into_inner` followed by `into_inner` (as `into_inner`). It follows a binding through imports, `use ... as` aliases, clones, moves and shadowing. Anything else leaves the binding and all of its clones unchanged, and `fix` names the binding and the section of the guide to read: a guard held across statements, a lock held across an await, two locks held at once, a call under the guard to a function or to a method of anything but the state, or formatting anything but the state and literals (an actor's callback panics if it uses an actor, and such a call might), or a use it can't follow, such as passing the binding to a function, storing it in a field `fix` doesn't rewrite, or naming it in a macro it can't read. The state's own methods and trait implementations, and operators and dereferences on any value, are assumed not to use an actor. Because the function that locks a binding might itself be called inside an actor's callback, where the binding's new actor would panic, `fix` also scans the whole package and leaves the binding for review if a function that locks it might run there: when it is locked in a closure, an async block, an async function or a trait method, through a public function of a library, or by a function used as a value; when an existing actor callback in the package calls anything; or when a statement that becomes a callback calls a function that locks it. A binary's functions can't be called from another crate, so their locals are the easiest to rewrite.

A private struct field of type `Arc<Mutex<T>>` becomes an `Actor<T>` the same way, together with the locals it is cloned into or set from. Its struct must be declared once in its file and derive nothing but `Clone` and `Default`, and no other field in the package may share its name. Its uses are followed through `self` in the struct's own `impl` blocks and through parameters and locals declared as the struct or a reference to it; a struct literal may set it to a constructor, a clone or a local. The same package scan leaves it for review if a function that locks it, or one of those locals, might run inside an actor's callback.

A parameter of type `Arc<Mutex<T>>`, `&Arc<Mutex<T>>` or `&Mutex<T>`, and a return type of `Arc<Mutex<T>>`, become `Actor<T>`, `&Actor<T>` or `Actor<T>` when the function is private and isn't async, `extern` or a trait's. Every call must name the function directly in its file: `f(..)`, or `self.f(..)`, `Self::f(..)` or `Type::f(..)` in an `impl` of its type. Each argument must be a local or field `fix` follows, moved, cloned or borrowed; the result must be bound by a `let`, set a field `fix` rewrites or be returned; and the function must return a constructor or a value `fix` follows. The parameter, the return value and every value passed or taken stand or fall together, so a lock reached through a parameter counts for the caller's state too. A function named anywhere else in its package, even in another file, a macro or a string, is left for review, and so is `clone` on a `&Mutex<T>`, which copies the reference where an actor's would copy the actor. `RwLock` is left for review.

`fix --dry-run` prints a unified diff to stdout and leaves source files unchanged; Cargo may still update its lockfile and build artifacts. Both forms accept `--strict` and Cargo arguments after `--`. A real fix runs `check` again: exit status is `0` when clean, `1` for remaining findings, and `2` for a build failure. Dry runs report the status before fixes. Review the diff and test the result before committing.

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

### Experimental execution-context checks

An optional [Dylint prototype](lints/README.md) detects known blocking operations
inside async execution and nested `Actor::with`/`read`/`snapshot` access. It needs
an explicitly built library and a separate pinned nightly toolchain:

```sh
cargo adamantium check --dylint-library /absolute/path/to/the/built/library
```

Ordinary checks and `init` keep their existing toolchain. See the prototype's
setup, coverage limits, and measurements before opting in. A failed experimental
check returns exit 2; it never silently falls back to Clippy-only success.

### Strict tier

Use `--strict` to add stricter rules to the default set:

```sh
cargo adamantium check --strict
cargo adamantium init --strict
cargo adamantium rules --strict              # also supports --format json or toml
```

The strict tier starts with [`arithmetic_side_effects`](https://clippy.rust-lang.org/master/index.html#arithmetic_side_effects). It flags arithmetic operators that can overflow or panic, including integer division and negation. Use methods such as `checked_add`, `checked_div`, `saturating_add`, or `wrapping_add` to make the intended behavior explicit. Clippy recognizes some safe operations, but cannot prove every application's arithmetic bounds, so this tier is opt-in.

`init --strict` adds the strict lint levels to the project's tables so plain `cargo clippy` applies them. Keep passing `--strict` to `check` in CI to include those rules in Adamantium's reports. Repeating `init --strict` makes no changes; running `init` afterward preserves previously added strict lint levels. The same skip lists apply to both tiers.

### Dependencies

The rules stop at the crate boundary. `deps` shows what a project's guarantees rest on beyond it: which dependencies use `unsafe` code, and which call the paths that `clippy.toml` disallows, such as `tokio::spawn`, `std::thread::spawn`, `std::thread::sleep` and `std::mem::forget`.

```sh
cargo adamantium deps
cargo adamantium deps --format json -- --all-features
```

It reads the paths from the same `clippy.toml` that `check` applies, so the report and the rules cover the same calls. It takes the dependencies from `cargo metadata` for the host target, leaving out workspace members and dev-dependencies, and reads each library's source with syn, without building it. Arguments after `--` go to `cargo metadata` and `cargo tree`, to choose features. Features come from a normal build: features that only dev-dependencies turn on, and the packages they pull in, are left out, and a feature that is on in only some of the builds of a package, such as its build for a build script, is treated as unknown. Runtime dependencies are listed apart from the ones that only run while compiling: build dependencies, procedural macros, and what only they depend on.

For each dependency, the report counts `unsafe` functions, blocks, impls, traits and `extern` blocks, the attributes and `global_asm!` calls that the `unsafe_code` lint reports, and `unsafe` in macro bodies that syn can't parse, such as the `macro_rules!` definitions it doesn't expand. A plain `extern` block, which editions before 2024 allow, isn't counted: its functions can only be called in `unsafe` blocks, which are. The attributes are rustc's unsafe attributes, such as `no_mangle`, `export_name`, `link_section` and `naked`, and `allow_internal_unsafe`, counted with or without the `unsafe(...)` that the 2024 edition requires, including those a `cfg_attr` adds. A `global_asm!` call is counted through any path or `use` that leads to `core::arch::global_asm`, and by the name `global_asm` where a glob import may provide it, but not where a `macro_rules!` of that name in textual scope hides it. In a macro body, such an attribute written without `unsafe(...)`, and a `global_asm!` call, are counted with the `unsafe` there. It notes a library that forbids `unsafe_code`, by attribute or in its manifest's `[lints]`. It lists each disallowed path the dependency uses, with the file, line and column of each use.

The scan follows the library's module tree, so tests, examples and benches are left out. It evaluates `cfg` with the resolved features and the host target, so `#[cfg(test)]` code and `#[test]` functions are left out too. Code behind a configuration that can't be known before building, such as `debug_assertions` or a `--cfg` that a build script sets, is counted. Where items or `use` items of one name each depend on such a configuration, any of them may be the one built, so a path through the name counts for each. A module whose `#[path]` depends on such a configuration is read from each place a build may take it from that has a file. Paths are resolved through `use` items, re-exports within the crate, and macro bodies that parse as items, statements or expressions. A `cfg_if!` or `cfg_select!` among a module's items is read as its branches: each branch's items, and the modules it declares, are scanned where the branch may be the one built. A module-level call of a `macro_rules!` that the crate defines in the call's textual scope, such as serde's `crate_root!();` or Tokio's `feature! { #![unix] mod symlink; ... }`, is read as its expansion: the call's input is matched against the macro's rules in order, as rustc matches it, and the items of the first rule that matches are scanned, with the modules they declare and the `cfg` the rule puts on them. The code is counted once for each call, and its uses are reported in the file they are written in, the call's or the macro's. Where the definition depends on an unknown `cfg`, each definition the call may name is read under the `cfg` that selects it. A call isn't expanded where the scan can't match it as rustc would without compiling: where matching reaches an `expr`, `stmt` or `pat` fragment, whose tokens syn and rustc parse differently. Expansions are followed as deep as rustc's default `recursion_limit`, 128 calls, and for at most 10,000 calls in a crate; a call past either limit is listed as unscanned and read as tokens. A call of a macro that a `#[macro_use] extern crate` brings in, such as stacker's `psm_stack_manipulation!` from psm, is read the same way where no macro of its name is in textual scope: the dependency's `#[macro_export]` macros are read from its source with the features and `cfg`s of its own build, so where psm defines the macro twice under a `cfg` its build script sets, the call is read as each, and `$crate` in them names the dependency. Where two such items bring in a macro of one name, the later one's hides the earlier one's, as in rustc. A call of a dependency's macro by path, such as `psm::psm_stack_manipulation! { ... }` or `$crate::inner!` in another crate's macro, is read the same way, and so is a call by a name that a `use` in the calling module binds to one, such as `use psm::psm_stack_manipulation;`, which comes after textual scope and before the `#[macro_use]` macros. The path is followed through the dependency's `use` items, such as `pub use crate::m as renamed;`, to its root, where `#[macro_export]` puts the macro. A glob import may bring in a macro too, so a call in a module that has one isn't given the `#[macro_use]` macros. What the dependency's macro writes itself counts for the dependency, not for the crate that calls it, as rustc's `unsafe_code` lint leaves out code another crate's macro writes; the code the call passes in counts where it is written. A call of the crate's own macro by path or `use` is read the same way: its `#[macro_export]` macros are at its root, as `crate::m!` or `$crate::helper!` in another of its macros names them, and a `use` of a macro in textual scope, as `pub(crate) use m;` after `macro_rules! m`, puts it where the `use` is, as `crate::macros::m!` names it. A path through a module declared after the call, or to a macro the crate defines after it, is followed too: once the module tree is read, each call by path is resolved again in the whole tree, and where one would name other macros, the tree is read again, resolving paths through the names the last reading bound, until every call names what the tree says, at most 8 times. Calls in functions aren't expanded either, nor are calls of a macro that a dependency re-exports from another crate, such as `futures::select!` from futures-util; where a macro has such a call, its body is read as tokens too. A call that can't be tied to a path is counted by name as unresolved instead: a method call on a value, a name that a glob import may provide, a call in a macro body that doesn't parse, or a path into a crate that no entry names, which may re-export a disallowed item. Unresolved counts therefore include method calls such as `insert` and `remove` on any type. Source that can't be read is listed as unscanned with the reason, such as an `include!` of generated code, a missing module file, a `mod name;` in a macro body that doesn't parse, or a `#[path]` that depends on an unknown `cfg`, on an inline module or where no place it may name has a file. Code that a procedural macro generates is not seen.

The text report prints a table and then each dependency's calls on stdout. With `--format json`, it prints an object with `schema_version: 1`, `root`, `target`, and a `dependencies` array. Each dependency has `name`, `version`, `source`, `kind` (`runtime` or `build`), `forbids_unsafe`, the `unsafe` counts, `calls` with each path's `reason` and `sites`, `unresolved` counts by name, and `unscanned`. The exit status is `0` when the report is printed and `2` on an error.

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

The GitHub Action installs the CLI from a pinned commit and caches the binary. Check out your project and install Rust with Clippy first:

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: dtolnay/rust-toolchain@stable
    with:
      components: clippy
  - uses: xipkit/adamantium@main # Pin the action to a commit in production.
    with:
      rev: bfa0f78916cebb6c65cfaeefce45ed34339d324f # CLI commit
      # working-directory: path/to/project
```

`rev` must be a full commit SHA. Update it to upgrade the CLI; the cache is separated by commit, runner platform, and Rust compiler. The action requires Git, Cargo, and Clippy on the runner.
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
- **`first_ok`** takes items the way `concurrently` does and returns the first success, aborting and draining the items still running. A failure only frees a place for the next item; if every item fails, all the errors come back in input order. A limit of one tries the items in turn, and a limit as long as the input races them.
- **`blocking`** runs a closure on the blocking pool and resumes any panic on the calling task.

- **`Actor<S>`** shares state between tasks. `with` provides exclusive access and `read` provides read-only access through synchronous closures, so neither holds the lock across an await. A read-only method such as `Tally::total` can be passed to `read`. Calling `with` or `read` from inside either method on any actor panics to prevent deadlocks.
- **`AsyncActor<M>`** sends typed messages to a single worker inside `Tasks`. The worker owns its state and awaits each handler before taking the next message, allowing exclusive access through I/O without a shared-state lock. Its bounded mailbox applies backpressure. Call `close()` before successful scope completion to reject new sends and drain accepted work; dropping mailbox handles alone does not close it.
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

`Reply::pair()` makes a one-shot **`Reply<T>`** and the **`ReplyReceiver<T>`**
that waits for it, for request and reply between tasks. `send(value)` consumes
the reply and never waits; it hands the value back in `Closed(value)` if the
receiver is gone. `receive()` returns the value, or `None` if the reply was
dropped without sending. The replying side can check `is_closed()` or await
`closed()` to stop work whose answer nobody wants.

**`Watch<T>`** holds the latest value of some state, such as configuration.
`send(value)` replaces it and wakes every **`Watcher<T>`** made with
`subscribe()`. A watcher's `receive()` waits for a value it hasn't seen and
returns a copy, skipping values sent in between; `wait_for(condition)` waits for
a value that satisfies `condition`, starting with the current one; `get()` reads
the current value. Clones of a `Watch` can all send. Dropping the last one, or
calling `close()`, closes it: each watcher receives the last value if it hasn't
seen it, then `None`.

Both work with `default-features = false`. Both receivers also have
`try_receive()` and, with the `tokio` feature, `receive_until(&cancellation,
deadline)`, which follows the channel's rules.

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
`time::now()` use Tokio's clock, including its paused test clock, outside the
deterministic harness. Inside an execution they use virtual time without real
timers: time advances to the earliest pending deadline only when no application
task can run, and all timers at that instant become ready together.
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

### State that must await

Use `Actor::with` for synchronous shared-state updates. Use `AsyncActor` when
the state itself owns I/O that must not interleave, such as a connection with
an in-flight protocol exchange:

```rust
use adamantium::{AsyncActor, Tasks};

#[tokio::main]
async fn main() -> Result<(), &'static str> {
    Tasks::run(async |tasks| {
        let actor = AsyncActor::new(tasks, Vec::new(), 4, |state, message: u64| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                state.push(message);
                Ok(())
            })
        });
        actor.send(42).await.map_err(|_| "closed")?;
        actor.close();
        Ok(())
    }).await
}
```

The handler returns a boxed `Send` future borrowing `&mut state`. State and
messages need `Send + 'static`, but not `Clone` or `Sync`. Capacity bounds
queued messages, plus one being handled; it does not bound waiting producers.
Zero capacity means one. Concurrent senders are ordered only by enqueue order.
A successful `send` means accepted, not processed or durably committed.
Closed sends return the original message. Reply channels can be fields in
message types; the mailbox does not expose a receive operation.

Handler errors close the mailbox, discard queued messages, and propagate at
scope join through `Tasks`. They do not interrupt a pending scope body.
Panics and cancellation also close and discard the queue, even before the
worker's first poll and with surviving mailbox handles. Cancellation can
interrupt I/O and cannot roll back external effects or guarantee delivery.
Await scope shutdown to wait for cleanup; simply dropping a scope requests
aborts without waiting. A handler must not wait on its own worker to advance,
directly or through another mailbox: mailbox cycles can deadlock.

`cargo run --example async_actor` drives a real local TCP echo connection,
checks explicit replies, then closes and drains the mailbox and server.

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

Actor, Channel, Reply, Watch, Tasks and Cancellation support deterministic
scheduling, including synchronous
channel operations, competing senders and receivers, streams, close-and-drain,
endpoint drops and cancellation by dropping a pending future. Channel and Watch
share a private register-then-recheck notification boundary. Channel's bounded
storage uses a concurrent ring in production and a locked FIFO in the harness;
its notification, closure and endpoint ownership logic are shared. The harness schedules registration, notification,
polling and cancellation handoff, treating event-listener operations as atomic
notification steps rather than modeling event-listener's internal atomics.
Tasks preserve result claiming, oldest unclaimed errors, panic precedence and
abort/drain cleanup, including concurrent maps and ordered/unordered streams.
Blocking work models the queued/started boundary: queued closures can be
aborted, while started closures finish before a scope joins. Replace real
blocking I/O with deterministic fixtures using the modeled primitives.
`testing::parallel` also provides independent test workers. Cancellation-aware
operations accept virtual deadlines built from `time::now()`. Dropping a sleep
unregisters its timer. Sleeps belong to the execution where first polled;
carrying a pending sleep across executions is rejected. The opaque clock origin
is shared within a process, while replay across processes preserves elapsed
virtual durations. `Instant` itself carries no execution identity, so construct
deadlines inside each factory execution.
OS signal handling remains outside the harness. Use `#[test]`, not
`#[tokio::test]`, and create primitives inside the execution that uses them.
Replace I/O, real time, uncontrolled randomness and foreign executors with
deterministic fixtures. Arbitrary foreign futures cannot all be diagnosed, and
a step bound cannot interrupt a loop that makes no scheduling calls.

## The rules

The default lint levels are in this repository's `[workspace.lints]`, and the Clippy configuration is in [`cli/clippy.toml`](cli/clippy.toml). The repository's Cargo configuration points Clippy at that same bundled file. `cargo adamantium rules` prints both. The optional strict additions are recorded separately in `[workspace.metadata.adamantium-strict.lints]` and printed with `rules --strict`; they do not apply to this repository itself. The rules and library provide the following properties for code that follows them:

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

Tokio's `Semaphore` is allowed, and so is holding a permit across an await, because a permit guards no data. To cap how many items run at once, use `concurrently` or the streaming variants. Don't acquire a second permit while holding one: two tasks that do so in opposite orders deadlock.

Tests are exempt from the panic rules, so `unwrap`, indexing, and `panic!` are allowed in `#[test]` functions and `#[cfg(test)]` modules. Before Rust 1.100, Clippy does not treat helper functions in integration test files as tests, so those functions follow the rules. From Rust 1.100, they are exempt as well.

The rules ban `tokio::time::timeout`, which drops the future mid-await on expiry. Use `Tasks::run_until` with `Cancellation` and a deadline for scoped work, or `receive_until` and `send_until` for channel operations.

The rules have limits. `tokio::select!` and `futures::select!` are not banned yet because Clippy ignores scoped allowances for some macro expansions; the ban awaits [the upstream fix](https://github.com/rust-lang/rust-clippy/pull/17862) reaching stable. A plain `receive` or `send` can wait indefinitely while the channel stays open and empty or full; use `receive_until` or `send_until` to bound the wait with a `Cancellation` or a deadline. Dependencies can still use unsafe code or panic; [`cargo adamantium deps`](#dependencies) shows which use `unsafe` or call disallowed paths. Clippy cannot detect every call that blocks a worker thread. `Runtime::block_on` is allowed because `#[tokio::main]` and `#[tokio::test]` expand to it; called inside a runtime, it panics at run time instead of blocking. The default tier permits arithmetic that can panic; the strict tier lints potentially panicking arithmetic operators. Neither tier prevents stack exhaustion or panics inside called methods.

Atomics and `OnceLock` and `LazyLock` statics are allowed. An atomic holds no guard across an await and can't deadlock, so it suits a single counter or flag; Clippy can't see a check-then-act spread over two atomics, so state whose parts must agree belongs in one `Actor`. A `OnceLock` or `LazyLock` static is set once, but whatever in it can still change, such as an `Actor`, is outside every scope and shared by every task and every test in the binary. Prefer passing an `Actor` to the tasks that use it.

Strong reference cycles are not detected or collected. An `Actor` whose state owns a clone of itself, directly or through other actors, keeps every state in the cycle alive after the last external handle is dropped. Banning `Weak` and `Arc::downgrade` does not prevent strong cycles; keep actor ownership acyclic or explicitly remove cycle-forming handles before releasing the last external handle.

## Style review

`cargo adamantium style check` uses the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions) to review the judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). It returns findings for review; it does not edit source or replace rustfmt, Clippy, or code review.

```sh
cargo adamantium style check                      # every .rs file under the current directory
cargo adamantium style check src/lib.rs --context "application code"
cargo adamantium style check . --rule unnecessary_deep_clone --dry-run
cargo adamantium style check --changed=main --cache-from last.json > next.json
cargo adamantium style rules --format text
```

The command prints JSON by default, or a readable report with `--format text`. It finds Rust files recursively, respects ignore files, and skips hidden directories, `target`, and `node_modules`. It does not resolve Cargo workspace membership. Each file is sent to the provider in a request with the selected rules and any context. `--provider` selects the service that answers the rules; `decisions`, the OpenAI Decisions API, is the default and currently the only provider. Reports record the provider, and `--retry-from` refuses a report from a different one. For each file with findings, two more requests ask Decisions to locate them, since it answers only choices: first a window of lines, then the first and last line within it. A finding located with confidence has `lines` with `start_line` and `end_line`, numbered from 1; a finding without `lines` concerns the whole file, or Decisions could not place it. A file's `input_tokens` includes these requests; when locating fails, the file has no `input_tokens`, because the requests it made before failing may have been billed. The file, path, and context must fit within the API's 256 KiB JSON input limit. The exit status is `0` for a clean check or dry run, `1` for findings, and `2` for an incomplete check.

Transient failures are retried with exponential backoff and jitter, up to four attempts within three minutes per file. `Retry-After` is honored up to 30 seconds. Authentication, quota, and spending-limit errors are not retried. A service failure stops further requests and marks the remaining files as unchecked. Use `--retry-from previous.json` to retry failed and unchecked files in a new report. Supply the same scan root and `--rule` options, and pass `--context` again if needed. Do not redirect the output over the report being read.

Each request is billable, so two options keep a run to the files that need it. `--changed` reviews only the Rust files that differ from `HEAD`: staged, unstaged, and untracked files that are not ignored. `--changed=BASE` adds the files committed since the current branch left `BASE`, such as `--changed=main`. Deleted files are left out. `--cache-from previous.json` reuses a previous report's results for each file whose source has the same SHA-256 and was answered under the same provider, model, rule definitions, and context. Each report records those as `source_sha256` per file and `rules_sha256` and `context_sha256` for the run; editing a rule's wording changes `rules_sha256`. Reused files keep their findings, are marked `"cached": true`, and have no `input_tokens`. A report lists reused files alongside reviewed ones, so it can serve as the cache for the next run. Files that failed or were never sent are reviewed again. The cached report must have schema version 3 and the same scan root. `--retry-from` cannot be combined with either option.

Set `OPENAI_API_KEY`. On macOS, the `openai-api-key` Keychain item is used when the variable is unset. Source files and context are sent to OpenAI. Results are model estimates. Evaluate them on labeled examples before using them as a blocking check. The questions instruct the model to treat source comments and strings as untrusted evidence, but this does not guarantee resistance to adversarial input.

The rules are bundled in [`cli/src/style/google-rust-decisions.json`](cli/src/style/google-rust-decisions.json), with their guide coverage in [`style/rules/coverage.md`](style/rules/coverage.md) and labeled examples in [`style/fixtures/google-rust-cases.json`](style/fixtures/google-rust-cases.json). Run `node style/scripts/evaluate-decisions.mjs` to evaluate the examples against the API. It also accepts `--dry-run` and `--rule`. The full run makes 114 billable requests.

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

## Releasing

Both crates use the MIT license and share the version in `[workspace.package]`.
Pull requests run `cargo publish --dry-run` for each crate to verify that its
archive builds independently of this checkout. To inspect the archives locally:

```sh
cargo package --list -p adamantium
cargo package --list -p cargo-adamantium
cargo publish --locked --registry crates-io -p adamantium --all-features --dry-run
cargo publish --locked --registry crates-io -p cargo-adamantium --dry-run
```

Pushing a tag named `v` followed by the workspace version, such as `v0.1.0`,
starts the [publish workflow](.github/workflows/publish.yml). It checks the tag
against both crate versions, runs the Rust tests and Clippy, and verifies both
archives before publishing. Each crate publishes in its own job so a failed
upload can be retried without uploading the other crate again. The publish jobs
require the `CARGO_REGISTRY_TOKEN` repository secret.

The first publication and credential setup are tracked by ADAM-136 and require
Mauricio's go-ahead. Prepare and review the release before pushing its tag:
published crate versions cannot be overwritten or deleted. Crate-name checks
and dry runs do not reserve a name.
