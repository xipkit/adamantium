// Exports symbols, and has no other unsafe code. Each attribute that
// `unsafe_code` reports is counted once, with or without `unsafe(...)`.

#[no_mangle]
pub extern "C" fn plain() {}

#[unsafe(export_name = "renamed")]
pub extern "C" fn written_unsafe() {}

#[link_section = ".data.exports"]
pub static SECTIONED: u32 = 0;

pub struct Exports;

impl Exports {
    #[no_mangle]
    pub extern "C" fn method() {}
}

pub fn outer() {
    #[no_mangle]
    extern "C" fn inner() {}
}

// A `cfg_attr` adds one where its predicate holds or can't be known, and
// none where it can't hold.
#[cfg_attr(all(), unsafe(no_mangle))]
pub extern "C" fn holds() {}

#[cfg_attr(loom, cfg_attr(all(), no_mangle))]
pub extern "C" fn unknown() {}

#[cfg_attr(all(), cold, no_mangle)]
pub extern "C" fn one_of_two() {}

#[cfg_attr(target_pointer_width = "8", no_mangle)]
pub extern "C" fn never() {}

// Not counted: the function isn't built.
#[cfg(any())]
#[no_mangle]
pub extern "C" fn absent() {}

// Counted once, though the invocation expands to two items.
macro_rules! items {
    ($($item:item)*) => { $($item)* };
}

#[no_mangle]
items! {
    pub fn first() {}
    pub fn second() {}
}

// Counted in macros: the `unsafe` of the attribute.
macro_rules! export {
    ($section:ident) => {
        #[unsafe(link_section = ".data.exported")]
        pub static $section: u32 = 0;
    };
}

export!(EXPORTED);
