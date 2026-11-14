//! Modules that the crate's own `macro_rules!` declare, as serde's
//! `crate_root!` and libc's `prelude!` do.

#[macro_use]
mod macros;

// Read as the expansion of `root!`, whose modules are this file's.
root!();

// Declares `driver` through `$crate::declare!`, the crate's own exported
// macro called by path.
backend!(driver);
