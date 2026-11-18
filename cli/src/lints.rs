//! The rules `cargo adamantium` applies: lint levels passed to Clippy and the
//! Clippy configuration file. The repository's own `Cargo.toml` carries the
//! default levels and a separate strict table, and a test keeps both in step.

use std::{borrow::Cow, fmt};

use anyhow::{Context, Result};
use serde::Serialize;
use toml_edit::{DocumentMut, Item};

/// Which tool owns a lint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Rust,
    Clippy,
}

/// How a lint is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Deny,
    Forbid,
}

/// One lint and why the fragment needs it.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Lint {
    pub tool: Tool,
    pub name: &'static str,
    pub level: Level,
    pub why: &'static str,
    #[serde(skip)]
    pub example: Example,
}

/// A finding and how to replace it within the fragment.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Example {
    pub replacement: &'static str,
    pub before: &'static str,
    pub after: &'static str,
}

/// Names registered by the opt-in Dylint library, excluded from `init`.
pub const EXPERIMENTAL_LINTS: &[&str] = &[
    "adamantium_blocking_in_async",
    "adamantium_nested_actor_access",
    "adamantium_race_with_timer",
];

/// The rule name for the release profile check, which is not a Clippy lint.
pub const OVERFLOW_RULE: &str = "adamantium::overflow_checks";
pub const OVERFLOW_WHY: &str = "the release profile must set overflow-checks = true, so integer overflow panics instead of wrapping";
pub const OVERFLOW_EXAMPLE: Example = example(
    "Set overflow-checks = true in the root Cargo.toml release profile; cargo adamantium init adds it.",
    "[profile.release]\noverflow-checks = false",
    "[profile.release]\noverflow-checks = true",
);

/// The Clippy configuration bundled with the CLI and used by this repository.
pub const CLIPPY_TOML: &str = include_str!("../clippy.toml");

/// The Clippy configuration for the selected tier. Strict mode adds entries
/// to the same Clippy lists, which are enabled by both tiers.
pub fn clippy_toml(strict: bool) -> Result<Cow<'static, str>> {
    if !strict {
        return Ok(Cow::Borrowed(CLIPPY_TOML));
    }
    let mut config: DocumentMut = CLIPPY_TOML.parse().context("parsing bundled clippy.toml")?;
    let additions: DocumentMut = include_str!("../clippy-strict.toml")
        .parse()
        .context("parsing bundled clippy-strict.toml")?;
    for (key, entries) in additions.iter() {
        let entries = entries
            .as_array()
            .with_context(|| format!("strict Clippy configuration has no {key} array"))?;
        let list = config
            .get_mut(key)
            .and_then(Item::as_array_mut)
            .with_context(|| format!("default Clippy configuration has no {key} array"))?;
        list.extend(entries.iter().cloned());
    }
    Ok(Cow::Owned(config.to_string()))
}

const fn example(replacement: &'static str, before: &'static str, after: &'static str) -> Example {
    Example {
        replacement,
        before,
        after,
    }
}

const fn rust(name: &'static str, level: Level, why: &'static str, example: Example) -> Lint {
    Lint {
        tool: Tool::Rust,
        name,
        level,
        why,
        example,
    }
}

const fn clippy(name: &'static str, why: &'static str, example: Example) -> Lint {
    Lint {
        tool: Tool::Clippy,
        name,
        level: Level::Deny,
        why,
        example,
    }
}

/// The default lints, in the order the tables print them.
pub const LINTS: &[Lint] = &[
    rust(
        "unsafe_code",
        Level::Forbid,
        "every guarantee below rests on safe Rust",
        example(
            "Use safe slice access, which returns None for a missing value instead of requiring an unsafe precondition.",
            r#"fn first(values: &[u32]) -> Option<&u32> {
    unsafe { Some(values.get_unchecked(0)) }
}"#,
            r#"fn first(values: &[u32]) -> Option<&u32> {
    values.first()
}"#,
        ),
    ),
    rust(
        "unused_must_use",
        Level::Deny,
        "a result or future was left unused; handle the result or await the future",
        example(
            "Return or handle Result values and await futures, so failures reach the caller or an enclosing adamantium::Tasks scope.",
            r#"fn load(path: &std::path::Path) {
    std::fs::read(path);
}"#,
            r#"fn load(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}"#,
        ),
    ),
    clippy(
        "await_holding_invalid_type",
        "a type listed in clippy.toml is held across an await",
        example(
            "End the lifetime of types configured in await-holding-invalid-types before awaiting. For shared state, use adamantium::Actor with a synchronous closure.",
            r#"// clippy.toml: await-holding-invalid-types = ["example::Guard"]
struct Guard;
async fn example() {
    let guard = Guard;
    std::future::ready(()).await;
    drop(guard);
}"#,
            r#"struct Guard;
async fn example() {
    {
        let guard = Guard;
        drop(guard);
    }
    std::future::ready(()).await;
}"#,
        ),
    ),
    clippy(
        "await_holding_lock",
        "a lock guard lives across an await, so a cancelled task could leave it held",
        example(
            "Use adamantium::Actor::with or Actor::read for synchronous state access, then await outside the closure.",
            r#"async fn example() {
    let state = std::sync::Mutex::new(0);
    let mut guard = state.lock().unwrap();
    std::future::ready(()).await;
    *guard += 1;
}"#,
            r#"async fn example() {
    let state = adamantium::Actor::new(0);
    std::future::ready(()).await;
    state.with(|value| *value += 1);
}"#,
        ),
    ),
    clippy(
        "await_holding_refcell_ref",
        "a RefCell borrow lives across an await",
        example(
            "Use an owned value, or adamantium::Actor for shared state; keep state access inside a synchronous closure.",
            r#"async fn example() {
    let state = std::cell::RefCell::new(0);
    let mut value = state.borrow_mut();
    std::future::ready(()).await;
    *value += 1;
}"#,
            r#"async fn example() {
    let state = adamantium::Actor::new(0);
    std::future::ready(()).await;
    state.with(|value| *value += 1);
}"#,
        ),
    ),
    clippy(
        "cast_possible_truncation",
        "an `as` cast can silently drop bits; convert with `try_from`",
        example(
            "Use TryFrom and return or handle the conversion error; Adamantium does not replace checked numeric conversions.",
            r#"fn narrow(value: u64) -> u8 {
    value as u8
}"#,
            r#"fn narrow(value: u64) -> Result<u8, std::num::TryFromIntError> {
    u8::try_from(value)
}"#,
        ),
    ),
    clippy(
        "cast_possible_wrap",
        "an `as` cast can silently change sign; convert with `try_from`",
        example(
            "Use TryFrom to reject a value that would wrap into a negative number.",
            r#"fn signed(value: u32) -> i32 {
    value as i32
}"#,
            r#"fn signed(value: u32) -> Result<i32, std::num::TryFromIntError> {
    i32::try_from(value)
}"#,
        ),
    ),
    clippy(
        "cast_sign_loss",
        "an `as` cast can silently lose a sign; convert with `try_from`",
        example(
            "Use TryFrom to handle negative input explicitly instead of silently losing its sign.",
            r#"fn unsigned(value: i32) -> u32 {
    value as u32
}"#,
            r#"fn unsigned(value: i32) -> Result<u32, std::num::TryFromIntError> {
    u32::try_from(value)
}"#,
        ),
    ),
    clippy(
        "dbg_macro",
        "debugging output left in code",
        example(
            "Remove temporary debugging output; return the value or use the application's logging when output is intentional.",
            r#"fn count(value: usize) -> usize {
    dbg!(value)
}"#,
            r#"fn count(value: usize) -> usize {
    value
}"#,
        ),
    ),
    clippy(
        "disallowed_macros",
        "unstructured selection with futures::select!, listed in clippy.toml",
        example(
            "Use adamantium::first_ok for the first success, Tasks::run_until with Cancellation for scoped work, or receive_until/send_until for channel operations.",
            r#"async fn example() -> Result<u32, &'static str> {
    use futures::FutureExt;

    let a = std::future::ready(Ok(1)).fuse();
    let b = std::future::ready(Ok(2)).fuse();
    futures::pin_mut!(a, b);
    futures::select! {
        result = a => result,
        result = b => result,
    }
}"#,
            r#"async fn example() -> Result<u32, Vec<&'static str>> {
    adamantium::first_ok([1, 2], 2, |value| async move { Ok(value) }).await
}"#,
        ),
    ),
    clippy(
        "disallowed_methods",
        "panicking index methods, unmodeled channel constructors, unstructured spawns, Tokio select!, drop-to-cancel timeouts, blocking calls, clocks outside adamantium::time, uncontrolled randomness, thread-local access, leaks and weak references, listed in clippy.toml",
        example(
            "Use adamantium::Array::remove/insert and split_at_checked for checked index operations, Channel for bounded queues, Reply for one reply, Topic or Fanout for delivery to every subscriber, and Watch for latest-value state. Use Tasks::run and Tasks::spawn for scoped tasks. Replace tokio::select! with adamantium::first_ok for the first success. Use Tasks::run_until with Cancellation for scoped work and receive_until/send_until for channel operations with cancellation or deadlines. Use adamantium::blocking for blocking work and adamantium::time::sleep or sleep_until for timers. Build deadlines with adamantium::time::now and periodic waits with a loop over adamantium::time::sleep_until. Use adamantium::random for draws that the deterministic harness can seed and replay. Pass values in, or share them through an Actor, instead of keeping them in a thread-local. Let resources drop and use owned values in place of weak references.",
            r#"async fn example() -> Result<u32, tokio::task::JoinError> {
    let task = tokio::spawn(async { 42 });
    task.await
}"#,
            r#"async fn example() -> Result<u32, std::convert::Infallible> {
    adamantium::Tasks::run(async |tasks| {
        let task = tasks.spawn(async { Ok(42) });
        tasks.value(task).await
    }).await
}"#,
        ),
    ),
    clippy(
        "disallowed_types",
        "shared identity, runtime borrow checking, raw locks, unmodeled synchronization and unscoped task sets, listed in clippy.toml",
        example(
            "Use Adamantium values (Str, Bytes, Array, Deque, Map, Set) for owned data, Actor for shared state, and Tasks for scoped concurrency. Use Topic, Fanout, Reply, Watch, Channel, Cancellation, or Limit to model the kind of synchronization the code needs.",
            r#"fn example() {
    let state = std::sync::Mutex::new(0);
    *state.lock().unwrap() += 1;
}"#,
            r#"fn example() {
    let state = adamantium::Actor::new(0);
    state.with(|value| *value += 1);
}"#,
        ),
    ),
    clippy(
        "exit",
        "ending the process skips every scope's cleanup; return an exit code from main",
        example(
            "Return std::process::ExitCode from main so destructors and scope cleanup can run.",
            r#"fn run() {
    std::process::exit(1);
}
fn main() {
    run();
}"#,
            r#"fn run() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}
fn main() -> std::process::ExitCode {
    run()
}"#,
        ),
    ),
    clippy(
        "expect_used",
        "a panic on None or Err; handle the case or return it",
        example(
            "Return or handle None/Err. Use ok_or for Option or ? for Result instead of panicking.",
            r#"fn required(value: Option<u32>) -> u32 {
    value.expect("missing value")
}"#,
            r#"fn required(value: Option<u32>) -> Result<u32, &'static str> {
    value.ok_or("missing value")
}"#,
        ),
    ),
    clippy(
        "future_not_send",
        "every future must be Send so any task can run on any worker thread",
        example(
            "Use Send data such as adamantium::Array instead of Rc across await points, and Actor for shared mutable state, so Tasks can run the future on any worker.",
            r#"pub async fn example() -> usize {
    let values = std::rc::Rc::new(vec![1_u32]);
    std::future::ready(()).await;
    values.len()
}"#,
            r#"pub async fn example() -> usize {
    let values = adamantium::Array::from([1_u32]);
    std::future::ready(()).await;
    values.len()
}"#,
        ),
    ),
    clippy(
        "indexing_slicing",
        "indexing can panic; use `get`",
        example(
            "Use get with an index or range and handle None. Adamantium Array and Bytes expose safe slice access through get.",
            r#"fn item(values: &adamantium::Array<u32>, index: usize) -> u32 {
    values[index]
}"#,
            r#"fn item(values: &adamantium::Array<u32>, index: usize) -> Option<u32> {
    values.get(index).copied()
}"#,
        ),
    ),
    clippy(
        "let_underscore_future",
        "a future assigned to `_` is dropped without being awaited",
        example(
            "Await the future so its work runs. For concurrent work, spawn it inside adamantium::Tasks and observe its result within the scope.",
            r#"async fn example() {
    let _ = async { println!("done"); };
}"#,
            r#"async fn example() {
    async { println!("done"); }.await;
}"#,
        ),
    ),
    clippy(
        "let_underscore_must_use",
        "a must-use value assigned to `_` is silently discarded; handle it or explicitly drop it",
        example(
            "Return or handle the must-use value instead of hiding it with let _. Propagate Result failures to the caller or an enclosing adamantium::Tasks scope.",
            r#"fn load(path: &std::path::Path) {
    let _ = std::fs::read(path);
}"#,
            r#"fn load(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}"#,
        ),
    ),
    clippy(
        "mem_forget",
        "leaks",
        example(
            "Let ownership end normally or call drop so destructors free the resource.",
            r#"fn example() {
    let value = String::from("temporary");
    std::mem::forget(value);
}"#,
            r#"fn example() {
    let value = adamantium::Str::from("temporary");
    drop(value);
}"#,
        ),
    ),
    clippy(
        "panic",
        "an explicit panic; return an error",
        example(
            "Return a Result for failure so the caller and an enclosing adamantium::Tasks scope can handle it.",
            r#"fn validate(valid: bool) {
    if !valid {
        panic!("invalid input");
    }
}"#,
            r#"fn validate(valid: bool) -> Result<(), &'static str> {
    if !valid {
        return Err("invalid input");
    }
    Ok(())
}"#,
        ),
    ),
    clippy(
        "rc_buffer",
        "an Rc or Arc around a growable buffer; use adamantium's values",
        example(
            "Use adamantium::Str, Bytes, Array or Deque instead of Rc/Arc around growable buffers; their clones share storage until mutation.",
            r#"fn copy(values: std::sync::Arc<Vec<u32>>) -> std::sync::Arc<Vec<u32>> {
    values.clone()
}"#,
            r#"fn copy(values: adamantium::Array<u32>) -> adamantium::Array<u32> {
    values.clone()
}"#,
        ),
    ),
    clippy(
        "rc_mutex",
        "an Rc around a Mutex; use adamantium::Actor",
        example(
            "Use adamantium::Actor for shared state; its synchronous closures prevent holding the lock across an await.",
            r#"fn increment(state: std::rc::Rc<std::sync::Mutex<u32>>) {
    *state.lock().unwrap() += 1;
}"#,
            r#"fn increment(state: adamantium::Actor<u32>) {
    state.with(|value| *value += 1);
}"#,
        ),
    ),
    clippy(
        "string_slice",
        "slicing a string can panic inside a character",
        example(
            "Use Str::get with a byte range and handle None for out-of-bounds or non-character boundaries; use chars when selecting characters.",
            r#"fn prefix(text: &adamantium::Str, end: usize) -> &str {
    &text[..end]
}"#,
            r#"fn prefix(text: &adamantium::Str, end: usize) -> Option<&str> {
    text.get(..end)
}"#,
        ),
    ),
    clippy(
        "todo",
        "unfinished code panics at runtime",
        example(
            "Implement the operation or return an explicit error until it is available, rather than panicking at runtime.",
            r#"fn load() -> Result<adamantium::Str, &'static str> {
    todo!("loading is not ready")
}"#,
            r#"fn load() -> Result<adamantium::Str, &'static str> {
    Err("loading is not ready")
}"#,
        ),
    ),
    clippy(
        "unimplemented",
        "unfinished code panics at runtime",
        example(
            "Implement the operation or return an explicit error until it is supported.",
            r#"fn load() -> Result<adamantium::Str, &'static str> {
    unimplemented!("unsupported source")
}"#,
            r#"fn load() -> Result<adamantium::Str, &'static str> {
    Err("unsupported source")
}"#,
        ),
    ),
    clippy(
        "unreachable",
        "an explicit panic; make the case impossible in the types or return an error",
        example(
            "Make the case impossible with an enum, or return an error when external input can reach it.",
            r#"fn parse(value: &str) -> Result<bool, &'static str> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => unreachable!("invalid choice"),
    }
}"#,
            r#"fn parse(value: &str) -> Result<bool, &'static str> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err("invalid choice"),
    }
}"#,
        ),
    ),
    clippy(
        "unwrap_used",
        "a panic on None or Err; handle the case or return it",
        example(
            "Return or handle None/Err. Propagate Result with ? so an enclosing adamantium::Tasks scope can observe failure.",
            r#"fn load(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap()
}"#,
            r#"fn load(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}"#,
        ),
    ),
];

/// Additional lints enabled by the strict tier.
pub const STRICT_LINTS: &[Lint] = &[clippy(
    "arithmetic_side_effects",
    "arithmetic can overflow or divide by zero; use checked, saturating or wrapping methods",
    example(
        "Use checked arithmetic to handle overflow or division by zero explicitly, or saturating or wrapping methods when those semantics are intended.",
        r#"fn add(left: u32, right: u32) -> u32 {
    left + right
}"#,
        r#"fn add(left: u32, right: u32) -> Option<u32> {
    left.checked_add(right)
}"#,
    ),
)];

/// The default lints, plus the strict additions when requested.
pub fn selected(strict: bool) -> impl Iterator<Item = &'static Lint> {
    LINTS
        .iter()
        .chain(STRICT_LINTS.iter().filter(move |_| strict))
}

impl Lint {
    /// The lint's name as Clippy and rustc report it.
    pub fn qualified(&self) -> String {
        match self.tool {
            Tool::Rust => self.name.to_owned(),
            Tool::Clippy => format!("clippy::{}", self.name),
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Deny => "deny",
            Level::Forbid => "forbid",
        })
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tool::Rust => "rust",
            Tool::Clippy => "clippy",
        })
    }
}

/// The lint tables as they appear in a `Cargo.toml`: under `workspace.lints`
/// for a workspace, under `lints` for a single package.
pub fn lints_toml(workspace: bool, strict: bool) -> String {
    let prefix = if workspace { "workspace." } else { "" };
    let mut out = String::new();
    for tool in [Tool::Rust, Tool::Clippy] {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{prefix}lints.{tool}]\n"));
        for lint in selected(strict).filter(|lint| lint.tool == tool) {
            out.push_str(&format!("{} = \"{}\"\n", lint.name, lint.level));
        }
    }
    out
}
