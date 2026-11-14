// A module and a `use` of an item in it, of one name, behind a `cfg` that
// can't be known, as in rustix's `mod statx; pub use statx::statx;`. A path
// through the name may mean the module or the `use`, and the `use` can't
// lead through itself.
#[cfg(spawner_custom)]
mod forget {
    pub use std::mem::forget;
}
#[cfg(spawner_custom)]
pub use self::forget::forget;

pub fn circular(value: Box<u8>) {
    forget(value); // std::mem::forget
}

// Two names that may each be a `use` of the other. A way through one that
// leads back to the other is cut short only while the other is being
// resolved, so what the one meant there doesn't hold where the call reaches
// it again: `x` may mean `c::stop`, which may be `other::stop`.
mod tangled {
    #[cfg(spawner_a)]
    use b as a;
    #[cfg(not(spawner_a))]
    use self::inner as a;
    #[cfg(spawner_b)]
    use a as b;
    #[cfg(not(spawner_b))]
    use self::other as b;
    use a as d;
    use d as c;

    mod inner {
        pub use std::mem::drop as go;
        pub use std::mem::drop as stop;
    }

    mod other {
        pub use std::mem::drop as go;
        pub use std::mem::forget as stop;
    }

    #[cfg(spawner_c)]
    use a::go as x;
    #[cfg(not(spawner_c))]
    use c::stop as x;

    pub fn tangled(value: Box<u8>) {
        x(value); // std::mem::forget
    }
}
