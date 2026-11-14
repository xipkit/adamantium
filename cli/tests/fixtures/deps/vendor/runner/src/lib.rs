//! Calls Tokio's `runtime!` by path, and in `imported` by the name a `use`
//! brings in. Read as either of its definitions, each call declares the
//! module of its arm, and `$crate` in it is Tokio.

tokio::runtime! {
    yes {
        mod fast;
    }
    no {
        mod slow;
    }
}

mod imported;
