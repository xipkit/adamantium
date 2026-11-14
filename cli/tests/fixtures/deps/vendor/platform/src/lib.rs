//! Modules declared inside macro invocations.

macro_rules! when {
    ($cfg:meta => $($item:item)*) => {
        $(#[cfg($cfg)] $item)*
    };
}

// A call is read as the rule it matches, so `matched.rs` is loaded, under
// the `cfg` the rule gives it, and `excluded`, which has no file, isn't.
when! { unix => mod matched; }
when! { windows => mod excluded; }

macro_rules! unless {
    ($cfg:expr => $($item:item)*) => {
        $($item)*
    };
}

// An `expr` fragment can't be matched without compiling the crate, so the
// scan can't load `hidden.rs` and lists it.
unless! { cfg!(windows) => mod hidden; }

use cfg_if::cfg_if;

// `cfg_if!` is read as its branches, and only the first whose predicate
// holds is built: `slow` is off, so `slow.rs`, which has no file, isn't
// loaded.
cfg_if::cfg_if! {
    if #[cfg(feature = "slow")] {
        mod slow;
    } else if #[cfg(feature = "fast")] {
        mod fast;
    } else {
        mod fallback;
    }
}

// Where a predicate can't be known, each branch may be the one built.
cfg_if! {
    if #[cfg(platform_custom)] {
        mod custom;
    } else {
        mod portable;
    }
}

mod selection;
