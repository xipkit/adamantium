// In 2015, `use` paths start at the crate root, where `extern crate` binds.
#[macro_use]
extern crate tokio as rt;

mod sub;

// Tokio's `runtime!`, read as either of its definitions, declares the module
// of its arm, and `$crate` in it is Tokio.
runtime! {
    yes {
        mod fast;
    }
    no {
        mod slow;
    }
}
