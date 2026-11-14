// Assembles at module level, and has no other unsafe code. Each
// `global_asm!` that `unsafe_code` reports is counted once, by any path or
// `use` that leads to it.

use core::arch::global_asm;
use std::arch::global_asm as assemble;

global_asm!("");
assemble!("");
core::arch::global_asm!("");
::std::arch::global_asm!("");

mod through_a_module {
    use core::arch;

    arch::global_asm!("");
}

pub use core::arch::global_asm as reexported;

mod through_a_reexport {
    crate::reexported!("");
}

// A function of the same name doesn't hide the macro, which is in another
// namespace.
mod beside_a_function {
    use core::arch::global_asm;

    pub fn global_asm() {}

    global_asm!("");
}

// A glob import may provide it.
mod through_a_glob {
    use core::arch::*;

    global_asm!("");
}

pub fn outer() {
    mod inner {
        core::arch::global_asm!("");
    }
}

macro_rules! items {
    ($($item:item)*) => { $($item)* };
}

items! {
    core::arch::global_asm!("");
}

// Not counted: it isn't built, or it is another macro.
#[cfg(any())]
global_asm!("");

macro_rules! assembler {
    ($template:literal) => {};
}

assembler!("");

// Counted in macros, by a path or `$crate`, but not the other macro.
macro_rules! assembled {
    () => {
        core::arch::global_asm!("");
        $crate::reexported!("");
        assembler!("");
    };
}

assembled!();
