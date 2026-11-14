macro_rules! root {
    () => {
        mod engine;

        // Read from `docs/format.rs`: a build without `facade_docs` would
        // take it from `format.rs`, which has no file.
        #[cfg_attr(facade_docs, path = "docs/format.rs")]
        mod format;

        pub struct Handle;

        unsafe impl Send for Handle {}

        pub fn start() {
            std::thread::spawn(|| ()); // std::thread::spawn
        }
    };
}

// An internal helper, called by path as `$crate::declare!`.
#[doc(hidden)]
#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        mod $name;
    };
}

macro_rules! backend {
    ($name:ident) => {
        $crate::declare!($name);
    };
}
