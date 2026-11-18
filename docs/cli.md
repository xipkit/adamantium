# The `cargo adamantium` command

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

## `check`

`check` runs `cargo clippy --all-targets` with the Adamantium rules and the Adamantium `clippy.toml`.

- It turns off Clippy's default lints.
- It runs the rules as warnings. A crate with findings still builds, so `check` can also check the crates that depend on it.
- It checks every workspace member. To select packages, put arguments after `--`. `check` passes them to `cargo clippy`.
- It requires `overflow-checks = true` in the release profile. Integer overflow then panics in release builds, as it does in debug builds.

### Exit status

| Status | Meaning |
| --- | --- |
| `0` | No findings |
| `1` | Findings |
| `2` | The project did not build |

The exit status is the same for every output format.

### Text output

`check` prints the diagnostics and a summary to stderr.

### JSON output

`--format json` prints a report to stdout. The report uses `schema_version: 3`.

Each finding has the rule, message, file, line, column, source, and an optional `remediation`.

- The source is the text of the line where the finding starts, without leading and trailing whitespace.
- A finding without a line has `null` for the line, column, and source.
- `remediation` combines the applicable replacement (from the disallowed path's reason in `clippy.toml` or the rule's explanation) with the migration guide section or `cargo adamantium explain` command for the finding. It is omitted when no guidance applies, and `check --baseline` ignores it when reading earlier `schema_version: 3` reports.

The `warnings` array holds warnings about the Adamantium Clippy configuration and about unknown, renamed, or removed lints. Each warning has a message, file, and line.

- Warnings do not change the status or the exit status.
- Configuration warnings and errors name the file `clippy.toml`. Their lines refer to the bundled configuration that `cargo adamantium rules` prints.
- Lint warnings without a primary span (`E0602` and `renamed_and_removed_lints`) have `file: null` and `line: null`. Each appears one time for each code and message, across all targets.
- The report leaves out compiler summaries. Other compiler warnings appear only in text output.

### SARIF output

`--format sarif` prints a [SARIF 2.1.0](https://docs.oasis-open.org/sarif/sarif/v2.1.0/sarif-v2.1.0.html) log to stdout. GitHub code scanning shows it as annotations on pull requests.

- Each finding is an `error` result for its rule. When a finding has `remediation`, its SARIF result carries `properties.remediation`.
- Build errors and warnings are tool notifications, not results. They raise no alerts.
- File URIs are relative to the root of the git repository that holds the workspace. Outside git, they are relative to the workspace root.
- A finding without a line, such as a missing `overflow-checks`, points at the first line of its file.

### GitHub code scanning

```yaml
permissions:
  security-events: write
steps:
  - uses: actions/checkout@v4
  - run: cargo install --locked cargo-adamantium
  # Findings exit with 1; let them through so code scanning can show them.
  - run: cargo adamantium check --format sarif > adamantium.sarif || [ $? -eq 1 ]
  - uses: github/codeql-action/upload-sarif@v4
    if: always()
    with:
      sarif_file: adamantium.sarif
      category: adamantium
```

## `fix`

`fix` applies the rewrites that are safe to automate. It reports the others with a link to the section of the [migration guide](migrating.md) that covers them, or, for a rule the guide doesn't cover, the `cargo adamantium explain` command for it. It uses the same Clippy run and the same skip list as `check`.

### How to run it

- `fix --dry-run` prints a unified diff to stdout. It does not change source files. Cargo can still update its lockfile and build artifacts.
- `fix` applies the rewrites, then runs `check` again.
- Both forms accept `--strict`, and Cargo arguments after `--`.

Review the diff and test the result before you commit.

### Exit status

| Status | Meaning |
| --- | --- |
| `0` | No findings remain |
| `1` | Findings remain |
| `2` | The project did not build, or the fixed project does not build or cannot be checked |

A dry run reports the status before the fixes.

### Safety

- `fix` changes nothing in a project that does not build.
- If the fixed project does not build, or cannot be checked, `fix` prints the errors. It then writes every file that it changed back to its original text.
- If something else edited, moved, or deleted a file during the check, `fix` leaves that file as it is and names it.

### Requirements

- The migration rewrites require a dependency named `adamantium` that is not optional and not inherited from the workspace.
- The Tokio rewrites also require the default features or an explicit `tokio` feature.

### Clippy suggestions and Tokio calls

`fix` first applies the Clippy suggestions that are `MachineApplicable`. It then rewrites these expressions, including calls through imported aliases:

- A call that Clippy resolves to `tokio::time::sleep` and that is awaited immediately.
- An immediate `tokio::task::spawn_blocking(f).await.unwrap()`.

`fix` leaves these for review:

- Compound callees, stored sleep futures, and blocking calls with explicit type arguments.
- Macro expansions, external source files, and conflicting suggestions.
- Decisions about task scopes or lock lifetimes.

### A local `Arc<Mutex<T>>`

`fix` changes a local `Arc<Mutex<T>>` into an `Actor<T>` when every use of the binding, and of every clone of it, is mechanical. `fix` follows a binding through imports, `use ... as` aliases, clones, moves, and shadowing.

These uses are mechanical:

| Use | Rewrite |
| --- | --- |
| The constructor | `Actor::new` |
| `Arc::clone` | A clone of the actor |
| A guard used within one statement | `with`, or `read` when the statement only reads. A method called on the guard is called on `(*m)`, which keeps its method lookup. |
| `lock().unwrap().clone()` | A guard statement, such as `with(\|m\| (*m).clone())`. Shorten it to `snapshot()` when the state is `Clone`. |
| `Arc::try_unwrap` or `Arc::into_inner`, followed by `into_inner` | `into_inner` |

Any other use leaves the binding and all its clones unchanged. `fix` then names the binding and the section of the guide to read. These uses are not mechanical:

- A guard held across statements.
- A lock held across an `await`.
- Two locks held at the same time.
- A call under the guard to a function, or to a method of anything but the state.
- Formatting of anything but the state and literals.
- A use that `fix` cannot follow. Examples: the binding is passed to a function, stored in a field that `fix` does not rewrite, or named in a macro that `fix` cannot read.

The reason for the call and formatting limits: an actor's callback panics if it uses an actor, and such a call might use one. `fix` assumes that these do not use an actor: the methods and trait implementations of the state, and operators and dereferences on any value.

`fix` also scans the whole package. The function that locks a binding might run inside an actor's callback, where the new actor would panic. `fix` leaves the binding for review in these cases:

- The binding is locked in a closure, in a future that `fix` cannot prove, or in a trait method.
- The binding is locked through a public function of a library, or by a function used as a value.
- An existing actor callback in the package calls anything.
- A statement that becomes a callback calls a function that locks the binding.

Another crate cannot call the functions of a binary. The locals of a binary are thus the easiest to rewrite.

### Locks in async functions

Async functions and async blocks that are awaited immediately need a separate proof of where they are polled.

- Each private async function or method must have a unique name. Every use of it must be a direct `f(...).await`.
- Every enclosing future must satisfy the same rule.
- The chain must end at the binary entry point, which must use the `main` macro of Tokio 1 and tokio-macros 2 from crates.io.

This first version accepts only this shape of project:

- One binary target.
- No modules, and no imports that shadow names.
- No other attributes on the entry point, and no other uses of it.
- The default macro options, or only `flavor = "current_thread"` or `flavor = "multi_thread"`.
- Package identities that Cargo can resolve offline from the lockfile.

These remain manual: opaque macros, custom executors, `spawn` and `block_on` boundaries, futures that are stored, returned, or boxed, function values, closures, and public library entry points.

If `fix` cannot prove a path, it refuses the connected field, locals, and clones together. The diagnostic names the first future boundary that it could not prove.

This proof does not cover async libraries in general. `fix` never puts an `await` inside an actor callback, and it never permits a guard that spans an `await`.

### A struct field

A private struct field of type `Arc<Mutex<T>>` becomes an `Actor<T>` under the same rules, together with the locals that it is cloned into or set from.

- The struct must be declared one time in its file.
- The struct must derive nothing but `Clone` and `Default`.
- No other field in the package can have the same name.

`fix` follows the field through `self` in the `impl` blocks of the struct. It also follows parameters and locals that are declared as the struct or as a reference to it. A struct literal can set the field to a constructor, a clone, or a local.

The package scan applies here too. `fix` leaves the field for review if a function that locks it, or one of those locals, might run inside an actor's callback.

### Parameters and return values

| Before | After |
| --- | --- |
| A parameter of type `Arc<Mutex<T>>` | `Actor<T>` |
| A parameter of type `&Arc<Mutex<T>>` or `&Mutex<T>` | `&Actor<T>` |
| A return type of `Arc<Mutex<T>>` | `Actor<T>` |

The function must be private. It must not be async, `extern`, or a trait method.

- Every call must name the function directly in its file: `f(..)`, or `self.f(..)`, `Self::f(..)`, or `Type::f(..)` in an `impl` of its type.
- Each argument must be a local or a field that `fix` follows. It can be moved, cloned, or borrowed.
- The result must be bound by a `let`, set a field that `fix` rewrites, or be returned.
- The function must return a constructor or a value that `fix` follows.

The parameter, the return value, and every value passed or taken are rewritten together or not at all. A lock reached through a parameter thus counts for the state of the caller too.

`fix` leaves these for review:

- A function that is named anywhere else in its package, including another file, a macro, or a string.
- `clone` on a `&Mutex<T>`. That call copies the reference, but the same call on an actor copies the actor.
- `RwLock`.

### Channels

`fix` changes a local Tokio oneshot or watch channel, and with `--strict` a bounded mpsc channel, when one `let (sender, receiver) = ...` statement binds its halves and every use of each half reads the same after the rewrite. `fix` follows a half through moves into tasks and other locals, clones of watch halves, and shadowing. The migration guide compares the APIs for [replies](migrating.md#a-single-reply), [latest values](migrating.md#the-latest-value) and [bounded queues](migrating.md#a-bounded-queue).

| Before | After |
| --- | --- |
| `oneshot::channel()` | `Reply::pair()` |
| `rx.await` with `ok`, `is_ok`, `is_err`, `unwrap_or`, `map_err(\|_\| e)`, or an `Ok(v)` and `Err(_)` pattern | `rx.receive().await` with `ok` removed, `is_some`, `is_none`, `unwrap_or`, `ok_or_else(\|\| e)`, or a `Some(v)` and `None` pattern |
| `rx.try_recv()` | `rx.try_receive()` |
| `watch::channel(v)` | `Watch::new(v)` and `subscribe()` |
| `rx.changed().await` with `ok`, `is_ok`, `is_err`, `unwrap_or`, `map_err(\|_\| e)`, or an `Ok(())` and `Err(_)` pattern | `rx.changed().await`, now `Watcher::changed`, with `ok` removed, `is_some`, `is_none`, `unwrap_or`, `ok_or_else(\|\| e)`, or a `Some(())` and `None` pattern |
| `*rx.borrow_and_update()` or `rx.borrow_and_update().clone()` | `rx.get()` |
| `mpsc::channel(n)`, with `n` a positive integer literal | `Channel::bounded(n).split()` |
| `rx.recv().await` | `rx.receive().await` |
| mpsc `rx.try_recv()` | `rx.try_receive()` |

`fix` leaves these channels for review, and names the reason:

- A half passed to a function, stored in a field, returned, borrowed, or named in a macro or a type.
- `unwrap` or `expect` on a send or a wait, which would panic with another message.
- A oneshot receiver awaited inside a closure or an async block without `move`, which would borrow it instead of moving it.
- A watch send that might run once no receiver is alive: one inside a closure or an async block, or after the original receiver is moved, dropped, or captured by a `move` block. Tokio drops a value sent while no receiver is alive, and a `Watch` keeps it.
- A watch send whose result is used, and a watch sender that is read, subscribed to, or asked about its receivers, for the same reason.
- A watch receiver read with `borrow` that also waits for changes. `borrow` doesn't mark the value seen, and `get` does.
- A watch receiver's `has_changed`, which in Tokio fails once the sender is gone.
- An mpsc capacity that isn't a positive integer literal. Tokio panics on zero, and `Channel` rounds it up to one.
- An mpsc channel where two sends might wait at once: a cloned sender, or a send that isn't awaited where it is called or that runs in a closure or an async block borrowing the sender. Tokio admits waiting sends in order, and `Channel` doesn't.
- An mpsc receive that isn't awaited where it is called, and the value of an mpsc receiver's `close`, which is a `bool` for `Channel`.
- `reserve`, `blocking_send`, `blocking_recv`, `recv_many`, `poll_recv`, `send_timeout`, `capacity`, `max_capacity`, an mpsc sender's `closed`, and `downgrade`.

## `init`

`init` adds the rules to a project. After that, `cargo clippy` applies them without the command.

`init` writes these items:

- The `[lints]` tables, or `[workspace.lints]` in a workspace.
- The overflow check in the release profile.
- A `clippy.toml`.

`init` leaves out the rules that the [skip list](#skip-list) names under `allow` or `exceptions`.

A second run makes no changes. `init` does not replace a different `clippy.toml` unless you pass `--force`.

## `rules` and `explain`

`cargo adamantium rules` lists the rules and the reason for each. For a machine-readable list, run `cargo adamantium rules --format json`.

`cargo adamantium explain <rule>` prints the reason for the rule, the recommended replacement, and an example before and after the fix. It does not load a project or use the network.

- It accepts the bare rule name or the qualified name.
- For the release profile check, use `overflow_checks` or `adamantium::overflow_checks`.
- `--format json` prints an object with `schema_version: 1`, `rule` (the qualified name), `why`, `replacement`, `before`, and `after`.
- The exit status is `0` for a known rule. For an unknown rule name, the exit status is `2` and an error goes to stderr.

## Skip list

A skip list turns off rules by path or by name. Use it to adopt the rules in steps.

Most findings in an existing async crate are `Arc<Mutex<T>>` and `tokio::spawn`. The [migration guide](migrating.md) shows how to rewrite each with `Actor` and `Tasks`.

Add the skip list to `Cargo.toml`:

```toml
[workspace.metadata.adamantium]
skip = ["tests/", "examples/"]              # skip findings in these paths
allow = ["indexing_slicing", "unwrap_used"] # skip these rules throughout the project

[workspace.metadata.adamantium.exceptions]
future_not_send = ["src/syntax/", "src/bytecode.rs"] # skip this rule in these paths
```

- For a single package, use `[package.metadata.adamantium]`.
- Paths are globs relative to the directory that contains `Cargo.toml`. A path without wildcards also covers its contents.
- Use the bare rule name or the qualified name that `cargo adamantium rules` prints. An unknown rule name is an error.

`check` prints a summary of the skipped findings. The JSON report counts them by rule in its `skipped` map. To see the findings of a rule, remove the rule from `allow`.

A project with a skip list must run `cargo adamantium check` in CI. A `[lints]` table cannot limit a rule to a path, so `cargo clippy` alone does not apply the skip list.

## Baselines

A baseline accepts the findings that exist today and fails only on new ones. Use it to adopt every rule at once without fixing every finding first.

Save a report, then check against it:

```sh
cargo adamantium check --format json > adamantium-baseline.json
cargo adamantium check --baseline adamantium-baseline.json
```

With `--baseline`, `check` leaves out the findings that the baseline already has.

- The status, the exit status, and the findings in the JSON and SARIF reports cover only new findings.
- Text output shows diagnostics only for new findings.
- The JSON report counts the matched findings by rule in its `baselined` map.
- `baseline_unmatched` is the number of baseline findings that the check did not find. It is `null` without a baseline.

Make a new baseline after you fix findings. The fixed findings then cannot come back.

### How a finding matches

A finding matches the baseline when three things are the same: the rule, the path relative to the project root, and the source text.

- `check` ignores line numbers, columns, and messages. Edits elsewhere in a file do not bring findings back. A different Clippy version does not bring findings back. You can write the baseline on another machine.
- A finding becomes new if you edit its line or move it to another file.
- When several findings have the same rule, path, and source text, only those above the baseline count are new.
- The skip list applies first.

The baseline must be a `schema_version: 3` report from a check that built. `check` refuses older reports and reports from failed builds.

## Strict tier

`--strict` adds stricter rules to the default set:

```sh
cargo adamantium check --strict
cargo adamantium init --strict
cargo adamantium rules --strict              # also supports --format json or toml
```

The strict tier includes [`arithmetic_side_effects`](https://clippy.rust-lang.org/master/index.html#arithmetic_side_effects). This lint reports arithmetic operators that can overflow or panic, including integer division and negation. Strict mode also reports [bounded Tokio `mpsc`, `Notify`, and `Semaphore`](rules.md#synchronization).

To state the behavior that you intend, use methods such as `checked_add`, `checked_div`, `saturating_add`, or `wrapping_add`.

Clippy recognizes some safe operations. It cannot prove the arithmetic bounds of every application, so this tier is optional.

- `init --strict` adds the strict lint levels to `Cargo.toml` and strict paths to `clippy.toml`. It can upgrade the bundled default configuration directly. Use `--force` to replace a custom Clippy configuration.
- Continue to pass `--strict` to `check` in CI. The Adamantium reports then include the strict rules.
- A second `init --strict` makes no changes. A later `init` keeps the installed strict configuration and lint levels.
- The same skip list applies to both tiers.

## Dependencies

The rules stop at the crate boundary. `deps` shows what the guarantees of a project depend on beyond that boundary. It reports two things for each dependency:

- Where the dependency uses `unsafe` code.
- Where the dependency calls a path that `clippy.toml` disallows, such as `tokio::spawn`, `std::thread::spawn`, `std::thread::sleep`, and `std::mem::forget`.

```sh
cargo adamantium deps
cargo adamantium deps --format json -- --all-features
```

`deps` reads the disallowed paths from the same `clippy.toml` that `check` applies. The report and the rules thus cover the same calls.

### Which dependencies it reads

- `deps` takes the dependencies from `cargo metadata` for the host target.
- It leaves out workspace members and dev-dependencies.
- It reads the source of each library with syn. It does not build the library.
- Arguments after `--` go to `cargo metadata` and `cargo tree`. Use them to select features.
- Features come from a normal build. `deps` leaves out features that only dev-dependencies turn on, and the packages that those features add.
- A feature can be on in only some builds of a package, such as the build for a build script. `deps` treats that feature as unknown.
- The report lists runtime dependencies separately from the dependencies that run only during compilation. Those are build dependencies, procedural macros, and the packages that only they depend on.

### What it counts as `unsafe`

For each dependency, the report counts these items:

- `unsafe` functions, blocks, impls, traits, and `extern` blocks.
- The attributes that the `unsafe_code` lint reports. These are the unsafe attributes of rustc, such as `no_mangle`, `export_name`, `link_section`, and `naked`, and also `allow_internal_unsafe`.
- `global_asm!` calls.
- `unsafe` in macro bodies that syn cannot parse, such as the `macro_rules!` definitions that the scan does not expand.

Details:

- A plain `extern` block is not counted. Editions before 2024 allow it, and its functions can be called only in `unsafe` blocks, which are counted.
- An unsafe attribute is counted with or without the `unsafe(...)` that the 2024 edition requires. An attribute that a `cfg_attr` adds is counted too.
- A `global_asm!` call is counted through any path or `use` that leads to `core::arch::global_asm`. It is also counted by the name `global_asm` where a glob import can provide it. It is not counted where a `macro_rules!` of that name in textual scope hides it.
- In a macro body, an unsafe attribute without `unsafe(...)` and a `global_asm!` call are counted with the `unsafe` in that body.

The report also shows when a library forbids `unsafe_code`, by attribute or in the `[lints]` table of its manifest.

### What it reports as calls

The report lists each disallowed path that the dependency uses, with the file, line, and column of each use.

`deps` resolves paths through `use` items, re-exports within the crate, and macro bodies that parse as items, statements, or expressions.

### What it leaves out

- Tests, examples, and benches. The scan follows the module tree of the library.
- `#[cfg(test)]` code and `#[test]` functions. The scan evaluates `cfg` with the resolved features and the host target.
- Code that a procedural macro generates. The scan cannot see it.

### Configurations that are unknown before a build

Some configurations are unknown before a build, such as `debug_assertions` or a `--cfg` that a build script sets.

- Code behind such a configuration is counted.
- Several items or `use` items of one name can each depend on such a configuration. Any of them can be the one that is built, so a path through the name counts for each.
- A module whose `#[path]` depends on such a configuration is read from each location that a build can use and that has a file.

### Macros

The scan expands some macro calls and reads the result as source:

- A `cfg_if!` or `cfg_select!` among the items of a module. The scan reads each branch that can be the one that is built.
- A module-level call of a `macro_rules!` macro that the crate defines. Examples are `crate_root!();` in serde and `feature! { #![unix] mod symlink; ... }` in Tokio.
- A call of a macro from a dependency. The call can come through `#[macro_use] extern crate`, through a path such as `psm::psm_stack_manipulation! { ... }`, or through a `use` of the macro.

The scan matches the input of a call against the rules of the macro in order, as rustc does. It scans the items of the first rule that matches.

Rules for the counts:

- The code of an expansion is counted one time for each call. Each use is reported in the file where it is written: the file of the call or the file of the macro.
- Code that a dependency's macro writes counts for that dependency, not for the crate that calls the macro. The `unsafe_code` lint of rustc works the same way. Code that the call passes in counts where it is written.
- Where a macro definition depends on an unknown `cfg`, the scan reads each definition that the call can name.

The scan does not expand these calls. It reads them as tokens:

- A call where the match reaches an `expr`, `stmt`, or `pat` fragment. syn and rustc parse those tokens differently.
- A call inside a function.
- A call of a macro that a dependency re-exports from another crate, such as `futures::select!` from futures-util.
- A call beyond the limits. The scan follows expansions to a depth of 128 calls, which is the default `recursion_limit` of rustc. It expands at most 10,000 calls in a crate. A call beyond either limit is listed as unscanned.

### Unresolved calls

Some calls cannot be connected to a path. The report counts them by name as unresolved:

- A method call on a value.
- A name that a glob import can provide.
- A call in a macro body that does not parse.
- A path into a crate that no entry names. That crate might re-export a disallowed item.

Unresolved counts thus include method calls such as `insert` and `remove` on any type.

### Unscanned source

The report lists source that the scan cannot read as unscanned, with the reason. Examples:

- An `include!` of generated code.
- A missing module file.
- A `mod name;` in a macro body that does not parse.
- A `#[path]` that depends on an unknown `cfg`, that is on an inline module, or that names no location with a file.

### Output

The text report prints a table on stdout, then the calls of each dependency.

`--format json` prints an object with `schema_version: 2`, `root`, `target`, and a `dependencies` array. Each dependency has these fields:

- `name`, `version`, and `source`.
- `path` for path dependencies, relative to the audit root when inside it and absolute otherwise. Their `source` is `null`.
- `kind`: `runtime` or `build`.
- `forbids_unsafe`.
- The `unsafe` counts.
- `calls`, with the `reason` and the `sites` of each path.
- `unresolved`: counts by name.
- `unscanned`.

Without a policy, the exit status is `0` when the report is printed, and `2` on an error.

### Rejecting new findings in CI

Save a complete schema-version-2 JSON report from a reviewed lockfile and commit it with the project. In CI, compare the current dependencies against that report using the same host target and feature flags.

```sh
cargo adamantium deps --format json -- --locked > deps-baseline.json
cargo adamantium deps --deny-new --baseline deps-baseline.json -- --locked
```

`--deny-new` checks the report's unsafe counts and disallowed paths that start unscoped tasks or threads, such as `tokio::spawn`, `smol::Executor::spawn`, and `std::thread::spawn`. Other disallowed calls remain visible in the audit. Run `--deny-new` without a baseline to check against zero findings. `--baseline` requires `--deny-new`.

A dependency is identified by its Cargo package name and source. Registry dependencies use the registry source. Git dependencies use the Git source and requested ref, ignoring the resolved commit. Path dependencies use the reported `path`. Versions of the same name and source share counts, including when several versions are present at once.

The policy compares occurrence counts by unsafe category, or by disallowed task path and source file. Moving a call within a file preserves its match. Replacing occurrences within a matching group can leave the count unchanged. An increase is a new finding, and a decrease is reported for information. Refresh the baseline after reviewing removals if you want CI to catch their return.

An unscanned entry or an unresolved call whose name matches a task-starting path makes a report incomplete for the policy. Other unresolved calls are informational. Coverage is limited to what the source audit can observe, as described above.

| Status | Meaning with `--deny-new` |
| --- | --- |
| `0` | The scan completed and found no increases. |
| `1` | The scan completed and found at least one increase. |
| `2` | The scan failed or had gaps, or the supplied baseline was unreadable, invalid, incomplete, or for a different host target. |

With `--format json`, the `policy` object gives the `status` (`clean`, `findings`, or `error`) and, after a complete comparison, `changes` with `added` and `removed` counts by finding. The dependency results still contain the full audit, including any scan gaps.

## Experimental execution-context checks

An optional [Dylint prototype](../lints/README.md) detects two things:

- Known blocking operations inside async execution.
- Nested `Actor::with`, `read`, or `snapshot` access.

It needs a library that you build explicitly, and a separate pinned nightly toolchain:

```sh
cargo adamantium check --dylint-library /absolute/path/to/the/built/library
```

- Ordinary checks and `init` keep their existing toolchain.
- Read the setup, coverage limits, and measurements of the prototype before you use it.
- A failed experimental check has exit status `2`. It never falls back silently to a Clippy-only success.

## GitHub Action

The GitHub Action installs the command from a pinned commit and caches the binary. Check out your project and install Rust with Clippy first:

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

- `rev` must be a full commit SHA. Change it to upgrade the command.
- The cache is separate for each commit, runner platform, and Rust compiler.
- The action requires Git, Cargo, and Clippy on the runner.
