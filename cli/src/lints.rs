//! The rules `cargo adamantium` applies: lint levels passed to Clippy and the
//! Clippy configuration file. The repository's own `Cargo.toml` carries the
//! same levels, and a test keeps the two equal.

use std::fmt::{self, Write};

use serde::Serialize;

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
}

/// The Clippy configuration, as shipped in the repository root.
pub const CLIPPY_TOML: &str = include_str!("../../clippy.toml");

const fn rust(name: &'static str, level: Level, why: &'static str) -> Lint {
    Lint {
        tool: Tool::Rust,
        name,
        level,
        why,
    }
}

const fn clippy(name: &'static str, why: &'static str) -> Lint {
    Lint {
        tool: Tool::Clippy,
        name,
        level: Level::Deny,
        why,
    }
}

/// Every lint, in the order the tables print them.
pub const LINTS: &[Lint] = &[
    rust(
        "unsafe_code",
        Level::Forbid,
        "every guarantee below rests on safe Rust",
    ),
    clippy(
        "await_holding_invalid_type",
        "a type listed in clippy.toml is held across an await",
    ),
    clippy(
        "await_holding_lock",
        "a lock guard lives across an await, so a cancelled task could leave it held",
    ),
    clippy(
        "await_holding_refcell_ref",
        "a RefCell borrow lives across an await",
    ),
    clippy(
        "cast_possible_truncation",
        "an `as` cast can silently drop bits; convert with `try_from`",
    ),
    clippy(
        "cast_possible_wrap",
        "an `as` cast can silently change sign; convert with `try_from`",
    ),
    clippy(
        "cast_sign_loss",
        "an `as` cast can silently lose a sign; convert with `try_from`",
    ),
    clippy("dbg_macro", "debugging output left in code"),
    clippy(
        "disallowed_methods",
        "unstructured spawns, blocking sleeps, leaks and weak references, listed in clippy.toml",
    ),
    clippy(
        "disallowed_types",
        "shared identity, runtime borrow checking, raw locks and unscoped task sets, listed in clippy.toml",
    ),
    clippy(
        "exit",
        "ending the process skips every scope's cleanup; return an exit code from main",
    ),
    clippy(
        "expect_used",
        "a panic on None or Err; handle the case or return it",
    ),
    clippy(
        "future_not_send",
        "every future must be Send so any task can run on any worker thread",
    ),
    clippy("indexing_slicing", "indexing can panic; use `get`"),
    clippy("mem_forget", "leaks"),
    clippy("panic", "an explicit panic; return an error"),
    clippy(
        "rc_buffer",
        "an Rc or Arc around a growable buffer; use adamantium's values",
    ),
    clippy("rc_mutex", "an Rc around a Mutex; use adamantium::Actor"),
    clippy(
        "string_slice",
        "slicing a string can panic inside a character",
    ),
    clippy("todo", "unfinished code panics at runtime"),
    clippy("unimplemented", "unfinished code panics at runtime"),
    clippy(
        "unreachable",
        "an explicit panic; make the case impossible in the types or return an error",
    ),
    clippy(
        "unwrap_used",
        "a panic on None or Err; handle the case or return it",
    ),
];

impl Lint {
    /// The lint's name as Clippy and rustc report it.
    pub fn qualified(&self) -> String {
        match self.tool {
            Tool::Rust => self.name.to_owned(),
            Tool::Clippy => format!("clippy::{}", self.name),
        }
    }

    /// The two arguments that set this lint's level on a `cargo clippy` line.
    pub fn flags(&self) -> [String; 2] {
        let flag = match self.level {
            Level::Deny => "-D",
            Level::Forbid => "-F",
        };
        [flag.to_owned(), self.qualified()]
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
pub fn lints_toml(workspace: bool) -> String {
    let prefix = if workspace { "workspace." } else { "" };
    let mut out = String::new();
    for tool in [Tool::Rust, Tool::Clippy] {
        if !out.is_empty() {
            out.push('\n');
        }
        // Writing to a String cannot fail.
        let _ = writeln!(out, "[{prefix}lints.{tool}]");
        for lint in LINTS.iter().filter(|lint| lint.tool == tool) {
            let _ = writeln!(out, "{} = \"{}\"", lint.name, lint.level);
        }
    }
    out
}
