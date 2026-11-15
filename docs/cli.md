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

`check` runs `cargo clippy --all-targets` with Adamantium's rules and `clippy.toml`. It disables Clippy's default lints and runs the rules as warnings, allowing crates with findings and their dependents to be checked. It checks every workspace member unless arguments after `--` select packages; those arguments are passed to `cargo clippy`.

The rules also require `overflow-checks = true` in the release profile. Integer overflow then panics in release builds, as it does in debug builds.

### Output

The command prints diagnostics and a summary to stderr. With `--format json`, it prints a report to stdout with the rule, message, file, line, column, and source for each finding. The source is the text of the line the finding starts on, with leading and trailing whitespace removed; it is `null`, like the line and column, for a finding without a line. The exit status is `0` when clean, `1` for findings, and `2` when the project did not build.

The JSON check report uses `schema_version: 3`. Its `warnings` array contains warnings about Adamantium's Clippy configuration and unknown, renamed, or removed lints, each with a message, file, and line. Warnings do not change the status or exit code. Configuration warnings and errors name the file `clippy.toml`; their lines refer to Adamantium's bundled configuration, which `cargo adamantium rules` prints. Lint warnings without a primary span (`E0602` and `renamed_and_removed_lints`) have `file: null` and `line: null` and appear once per code and message across targets. Compiler summaries are omitted. Other compiler warnings appear only in text output.

With `--format sarif`, it prints a [SARIF 2.1.0](https://docs.oasis-open.org/sarif/sarif/v2.1.0/sarif-v2.1.0.html) log to stdout, which GitHub code scanning shows as annotations on pull requests. Each finding is an `error` result for its rule. Build errors and warnings are tool notifications rather than results, so they raise no alerts. File URIs are relative to the root of the git repository holding the workspace, or to the workspace root outside git. A finding without a line, such as a missing `overflow-checks`, points at the first line of its file. The exit status is the same as for the other formats.

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

`fix` uses the same Clippy pipeline and skip configuration. It applies `MachineApplicable` suggestions first, then rewrites immediately awaited calls that Clippy resolves to `tokio::time::sleep` and immediate `tokio::task::spawn_blocking(f).await.unwrap()` expressions (including imported aliases). Migration rewrites require a nonoptional, noninherited dependency named `adamantium`; the Tokio rewrites also need default features or an explicit `tokio` feature. Compound callees, stored sleep futures, and blocking calls with explicit type arguments are left for review. It leaves macro expansions, external source files, conflicting suggestions, and decisions about task scopes or lock lifetimes for review, with links to the [migration guide](migrating.md).

`fix` also turns a local `Arc<Mutex<T>>` into an `Actor<T>` when every use of it, and of every clone of it, is one the guide marks as mechanical: the constructor, `Arc::clone`, a guard used within one statement (as `with`, or `read` when it only reads; a method called on the guard is called on `(*m)`, which keeps its method lookup), `lock().unwrap().clone()` (as a guard statement, such as `with(|m| (*m).clone())`; shorten it to `snapshot()` when the state itself is `Clone`), and `Arc::try_unwrap` or `Arc::into_inner` followed by `into_inner` (as `into_inner`). It follows a binding through imports, `use ... as` aliases, clones, moves and shadowing. Anything else leaves the binding and all of its clones unchanged, and `fix` names the binding and the section of the guide to read: a guard held across statements, a lock held across an await, two locks held at once, a call under the guard to a function or to a method of anything but the state, or formatting anything but the state and literals (an actor's callback panics if it uses an actor, and such a call might), or a use it can't follow, such as passing the binding to a function, storing it in a field `fix` doesn't rewrite, or naming it in a macro it can't read. The state's own methods and trait implementations, and operators and dereferences on any value, are assumed not to use an actor. Because the function that locks a binding might itself be called inside an actor's callback, where the binding's new actor would panic, `fix` also scans the whole package and leaves the binding for review if a function that locks it might run there: when it is locked in a closure, an unproved future or a trait method, through a public function of a library, or by a function used as a value; when an existing actor callback in the package calls anything; or when a statement that becomes a callback calls a function that locks it. A binary's functions can't be called from another crate, so their locals are the easiest to rewrite.

Async functions and immediately awaited async blocks have a separate polling-context proof. Every use of each uniquely named private async function or method must be a direct `f(...).await`; every enclosing future must satisfy the same rule. The chain must end at the actual binary entry using the real crates.io Tokio 1 / tokio-macros 2 `main` macro. This first pass accepts only a sole binary target, no modules or shadowing imports, no other entry attributes or uses, and either the default macro options or just `flavor = "current_thread"` / `"multi_thread"`. Cargo identity must be resolvable offline from the lockfile. Opaque macros, custom executors, spawn/block_on boundaries, stored/returned/boxed futures, function values, closures and public library entry points remain manual. An unproved path refuses the connected field, locals and clones together, with the first unproved future boundary in the diagnostic. This does not provide general async-library coverage; it never puts an await inside an actor callback or permits a guard spanning await.

A private struct field of type `Arc<Mutex<T>>` becomes an `Actor<T>` the same way, together with the locals it is cloned into or set from. Its struct must be declared once in its file and derive nothing but `Clone` and `Default`, and no other field in the package may share its name. Its uses are followed through `self` in the struct's own `impl` blocks and through parameters and locals declared as the struct or a reference to it; a struct literal may set it to a constructor, a clone or a local. The same package scan leaves it for review if a function that locks it, or one of those locals, might run inside an actor's callback.

A parameter of type `Arc<Mutex<T>>`, `&Arc<Mutex<T>>` or `&Mutex<T>`, and a return type of `Arc<Mutex<T>>`, become `Actor<T>`, `&Actor<T>` or `Actor<T>` when the function is private and isn't async, `extern` or a trait's. Every call must name the function directly in its file: `f(..)`, or `self.f(..)`, `Self::f(..)` or `Type::f(..)` in an `impl` of its type. Each argument must be a local or field `fix` follows, moved, cloned or borrowed; the result must be bound by a `let`, set a field `fix` rewrites or be returned; and the function must return a constructor or a value `fix` follows. The parameter, the return value and every value passed or taken stand or fall together, so a lock reached through a parameter counts for the caller's state too. A function named anywhere else in its package, even in another file, a macro or a string, is left for review, and so is `clone` on a `&Mutex<T>`, which copies the reference where an actor's would copy the actor. `RwLock` is left for review.

`fix --dry-run` prints a unified diff to stdout and leaves source files unchanged; Cargo may still update its lockfile and build artifacts. Both forms accept `--strict` and Cargo arguments after `--`. `fix` changes nothing in a project that doesn't build. A real fix runs `check` again: exit status is `0` when clean and `1` for remaining findings. If the rewritten project no longer builds, or can't be checked, `fix` prints the errors, writes every file it changed back to its original text and exits with `2`; a file that something else edited, moved or deleted while `fix` was checking is left as it is and named. Dry runs report the status before fixes. Review the diff and test the result before committing.

## `init`

`init` adds the rules to a project so plain `cargo clippy` applies them. It writes the `[lints]` tables, or `[workspace.lints]` in a workspace, the release profile's overflow check, and a `clippy.toml`. It excludes rules listed under `allow` or `exceptions` in the [skip list](#skip-list). Running it again makes no changes, and it will not replace a different `clippy.toml` without `--force`.

## `rules` and `explain`

For a machine-readable list of rules and their reasons, run `cargo adamantium rules --format json`.

`cargo adamantium explain <rule>` prints the rule's reasoning, recommended replacement, and before-and-after example without loading a project or accessing the network. It accepts the bare or qualified rule name, including `overflow_checks` or `adamantium::overflow_checks` for the release profile check. With `--format json`, it prints an object with `schema_version: 1`, `rule` (the qualified name), `why`, `replacement`, `before`, and `after`. Successful explanations exit with `0`; unknown rule names exit with `2` and an error on stderr.

## Skip list

Most findings in an existing async crate are `Arc<Mutex<T>>` and `tokio::spawn`. The [migration guide](migrating.md) shows how to rewrite each with `Actor` and `Tasks`.

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

## Baselines

The skip list adopts rules by path or by rule. To adopt them everywhere at once without fixing every existing finding first, save a report and check against it:

```sh
cargo adamantium check --format json > adamantium-baseline.json
cargo adamantium check --baseline adamantium-baseline.json
```

With `--baseline`, findings that the earlier report already has are left out, so only new findings fail the check. The status, the exit code, and the findings in JSON and SARIF reports cover only new findings, and text output shows diagnostics only for them. The JSON report counts the matched findings by rule in its `baselined` map, and `baseline_unmatched` is the number of findings in the baseline that the check did not find, or `null` without a baseline. Regenerate the baseline after fixing findings so they cannot come back.

A finding matches the baseline when its rule, its path relative to the project root, and its source text are the same. Line numbers, columns, and messages are ignored, so edits elsewhere in a file and a different Clippy version do not bring findings back, and the baseline can be written on another machine. Editing the line of a finding, or moving it to another file, makes it new. When findings share a rule, path, and source text, only those beyond the baseline's count are new. The skip list applies first. The baseline must be a `schema_version: 3` report from a check that built; `check` refuses older reports and reports from failed builds.

## Strict tier

Use `--strict` to add stricter rules to the default set:

```sh
cargo adamantium check --strict
cargo adamantium init --strict
cargo adamantium rules --strict              # also supports --format json or toml
```

The strict tier starts with [`arithmetic_side_effects`](https://clippy.rust-lang.org/master/index.html#arithmetic_side_effects). It flags arithmetic operators that can overflow or panic, including integer division and negation. Use methods such as `checked_add`, `checked_div`, `saturating_add`, or `wrapping_add` to make the intended behavior explicit. Clippy recognizes some safe operations, but cannot prove every application's arithmetic bounds, so this tier is opt-in.

`init --strict` adds the strict lint levels to the project's tables so plain `cargo clippy` applies them. Keep passing `--strict` to `check` in CI to include those rules in Adamantium's reports. Repeating `init --strict` makes no changes; running `init` afterward preserves previously added strict lint levels. The same skip lists apply to both tiers.

## Dependencies

The rules stop at the crate boundary. `deps` shows what a project's guarantees rest on beyond it: which dependencies use `unsafe` code, and which call the paths that `clippy.toml` disallows, such as `tokio::spawn`, `std::thread::spawn`, `std::thread::sleep` and `std::mem::forget`.

```sh
cargo adamantium deps
cargo adamantium deps --format json -- --all-features
```

It reads the paths from the same `clippy.toml` that `check` applies, so the report and the rules cover the same calls. It takes the dependencies from `cargo metadata` for the host target, leaving out workspace members and dev-dependencies, and reads each library's source with syn, without building it. Arguments after `--` go to `cargo metadata` and `cargo tree`, to choose features. Features come from a normal build: features that only dev-dependencies turn on, and the packages they pull in, are left out, and a feature that is on in only some of the builds of a package, such as its build for a build script, is treated as unknown. Runtime dependencies are listed apart from the ones that only run while compiling: build dependencies, procedural macros, and what only they depend on.

For each dependency, the report counts `unsafe` functions, blocks, impls, traits and `extern` blocks, the attributes and `global_asm!` calls that the `unsafe_code` lint reports, and `unsafe` in macro bodies that syn can't parse, such as the `macro_rules!` definitions it doesn't expand. A plain `extern` block, which editions before 2024 allow, isn't counted: its functions can only be called in `unsafe` blocks, which are. The attributes are rustc's unsafe attributes, such as `no_mangle`, `export_name`, `link_section` and `naked`, and `allow_internal_unsafe`, counted with or without the `unsafe(...)` that the 2024 edition requires, including those a `cfg_attr` adds. A `global_asm!` call is counted through any path or `use` that leads to `core::arch::global_asm`, and by the name `global_asm` where a glob import may provide it, but not where a `macro_rules!` of that name in textual scope hides it. In a macro body, such an attribute written without `unsafe(...)`, and a `global_asm!` call, are counted with the `unsafe` there. It notes a library that forbids `unsafe_code`, by attribute or in its manifest's `[lints]`. It lists each disallowed path the dependency uses, with the file, line and column of each use.

The scan follows the library's module tree, so tests, examples and benches are left out. It evaluates `cfg` with the resolved features and the host target, so `#[cfg(test)]` code and `#[test]` functions are left out too. Code behind a configuration that can't be known before building, such as `debug_assertions` or a `--cfg` that a build script sets, is counted. Where items or `use` items of one name each depend on such a configuration, any of them may be the one built, so a path through the name counts for each. A module whose `#[path]` depends on such a configuration is read from each place a build may take it from that has a file. Paths are resolved through `use` items, re-exports within the crate, and macro bodies that parse as items, statements or expressions. A `cfg_if!` or `cfg_select!` among a module's items is read as its branches: each branch's items, and the modules it declares, are scanned where the branch may be the one built. A module-level call of a `macro_rules!` that the crate defines in the call's textual scope, such as serde's `crate_root!();` or Tokio's `feature! { #![unix] mod symlink; ... }`, is read as its expansion: the call's input is matched against the macro's rules in order, as rustc matches it, and the items of the first rule that matches are scanned, with the modules they declare and the `cfg` the rule puts on them. The code is counted once for each call, and its uses are reported in the file they are written in, the call's or the macro's. Where the definition depends on an unknown `cfg`, each definition the call may name is read under the `cfg` that selects it. A call isn't expanded where the scan can't match it as rustc would without compiling: where matching reaches an `expr`, `stmt` or `pat` fragment, whose tokens syn and rustc parse differently. Expansions are followed as deep as rustc's default `recursion_limit`, 128 calls, and for at most 10,000 calls in a crate; a call past either limit is listed as unscanned and read as tokens. A call of a macro that a `#[macro_use] extern crate` brings in, such as stacker's `psm_stack_manipulation!` from psm, is read the same way where no macro of its name is in textual scope: the dependency's `#[macro_export]` macros are read from its source with the features and `cfg`s of its own build, so where psm defines the macro twice under a `cfg` its build script sets, the call is read as each, and `$crate` in them names the dependency. Where two such items bring in a macro of one name, the later one's hides the earlier one's, as in rustc. A call of a dependency's macro by path, such as `psm::psm_stack_manipulation! { ... }` or `$crate::inner!` in another crate's macro, is read the same way, and so is a call by a name that a `use` in the calling module binds to one, such as `use psm::psm_stack_manipulation;`, which comes after textual scope and before the `#[macro_use]` macros. The path is followed through the dependency's `use` items, such as `pub use crate::m as renamed;`, to its root, where `#[macro_export]` puts the macro. A glob import may bring in a macro too, so a call in a module that has one isn't given the `#[macro_use]` macros. What the dependency's macro writes itself counts for the dependency, not for the crate that calls it, as rustc's `unsafe_code` lint leaves out code another crate's macro writes; the code the call passes in counts where it is written. A call of the crate's own macro by path or `use` is read the same way: its `#[macro_export]` macros are at its root, as `crate::m!` or `$crate::helper!` in another of its macros names them, and a `use` of a macro in textual scope, as `pub(crate) use m;` after `macro_rules! m`, puts it where the `use` is, as `crate::macros::m!` names it. A path through a module declared after the call, or to a macro the crate defines after it, is followed too: once the module tree is read, each call by path is resolved again in the whole tree, and where one would name other macros, the tree is read again, resolving paths through the names the last reading bound, until every call names what the tree says, at most 8 times. Calls in functions aren't expanded either, nor are calls of a macro that a dependency re-exports from another crate, such as `futures::select!` from futures-util; where a macro has such a call, its body is read as tokens too. A call that can't be tied to a path is counted by name as unresolved instead: a method call on a value, a name that a glob import may provide, a call in a macro body that doesn't parse, or a path into a crate that no entry names, which may re-export a disallowed item. Unresolved counts therefore include method calls such as `insert` and `remove` on any type. Source that can't be read is listed as unscanned with the reason, such as an `include!` of generated code, a missing module file, a `mod name;` in a macro body that doesn't parse, or a `#[path]` that depends on an unknown `cfg`, on an inline module or where no place it may name has a file. Code that a procedural macro generates is not seen.

The text report prints a table and then each dependency's calls on stdout. With `--format json`, it prints an object with `schema_version: 1`, `root`, `target`, and a `dependencies` array. Each dependency has `name`, `version`, `source`, `kind` (`runtime` or `build`), `forbids_unsafe`, the `unsafe` counts, `calls` with each path's `reason` and `sites`, `unresolved` counts by name, and `unscanned`. The exit status is `0` when the report is printed and `2` on an error.

## Experimental execution-context checks

An optional [Dylint prototype](../lints/README.md) detects known blocking operations
inside async execution and nested `Actor::with`/`read`/`snapshot` access. It needs
an explicitly built library and a separate pinned nightly toolchain:

```sh
cargo adamantium check --dylint-library /absolute/path/to/the/built/library
```

Ordinary checks and `init` keep their existing toolchain. See the prototype's
setup, coverage limits, and measurements before opting in. A failed experimental
check returns exit 2; it never silently falls back to Clippy-only success.

## GitHub Action

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
