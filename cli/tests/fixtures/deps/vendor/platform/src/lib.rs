//! Modules declared inside macro invocations.

macro_rules! when {
    ($cfg:meta => $($item:item)*) => {
        $(#[cfg($cfg)] $item)*
    };
}

// The body isn't items, so the scan can't load `hidden.rs` and lists it.
when! { unix => mod hidden; }
