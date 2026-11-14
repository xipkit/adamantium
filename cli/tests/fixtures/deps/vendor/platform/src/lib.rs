//! Modules declared inside macro invocations.

macro_rules! when {
    ($cfg:meta => $($item:item)*) => {
        $(#[cfg($cfg)] $item)*
    };
}

// The body isn't items, so the scan can't load `hidden.rs` and lists it.
when! { unix => mod hidden; }

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
