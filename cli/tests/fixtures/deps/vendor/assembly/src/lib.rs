// Assembles at module level, and has no other unsafe code. Each
// `global_asm!` that `unsafe_code` reports is counted once, by any path or
// `use` that leads to it.

use core::arch::global_asm;
use std::arch::global_asm as assemble;

global_asm!("");
assemble!("");
core::arch::global_asm!("");
::std::arch::global_asm!("");

// A template whose input parses as items is still a template.
core::arch::global_asm!(concat! { "" });
assemble!(concat! { "" });

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

// Each copy of a call that a macro writes twice is read through its input,
// though only one is built. The tag keeps the scan from reading `twice!`
// through its input instead of expanding it.
macro_rules! passthrough {
    ($($item:item)*) => { $($item)* };
}

macro_rules! twice {
    (tag $item:item) => {
        #[cfg(any())]
        mod disabled {
            $item
        }
        mod enabled {
            $item
        }
    };
}

twice! {
    tag passthrough! {
        core::arch::global_asm!("");
    }
}

// Not counted: it isn't built, it is another macro, or a `macro_rules!` in
// textual scope hides it, with or without a `use` of it. Matching reaches
// an `expr`, so the scan doesn't expand those calls.
#[cfg(any())]
global_asm!("");

macro_rules! assembler {
    ($template:literal) => {};
}

assembler!("");

mod hidden_by_a_macro {
    macro_rules! global_asm {
        ($template:expr) => {};
    }

    global_asm!("");
}

mod hidden_by_a_macro_beside_a_use {
    #[allow(unused_imports)]
    use core::arch::global_asm;

    macro_rules! global_asm {
        ($template:expr) => {};
    }

    global_asm!("");
}

// The scan doesn't follow textual scope in a function, but nothing else
// binds the name, nor may a glob import.
pub fn hides_locally() {
    mod hidden_in_a_function {
        macro_rules! global_asm {
            ($template:expr) => {};
        }

        global_asm!("");
    }
}

// Nor in a macro body, read as its expansion or as tokens, where a
// `macro_rules!` in textual scope hides it.
mod hidden_in_a_macro_body {
    #[allow(unused_imports)]
    use core::arch::global_asm;

    macro_rules! global_asm {
        ($template:expr) => {};
    }

    macro_rules! wrapper {
        () => {
            global_asm!("");
        };
    }

    wrapper!();

    pub fn wraps() {
        wrapper!();
    }
}

// Counted in macros, where the scan reads the body as tokens since the call
// is in a function: by a path or `$crate`, but not the other macro.
macro_rules! assembled {
    () => {
        mod assembled {
            core::arch::global_asm!("");
            $crate::reexported!("");
            assembler!("");
        }
    };
}

pub fn assembles() {
    assembled!();
}
