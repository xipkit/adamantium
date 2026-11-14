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

// Counted in macros: the bare attributes, also with a metavariable for a
// value or a predicate, and the `unsafe` of the other, but not the one a
// `cfg_attr` can't add.
macro_rules! export {
    ($name:ident, $section:ident, $other:ident, $symbol:literal, $place:literal, $cfg:meta) => {
        #[no_mangle]
        pub extern "C" fn $name() {}
        #[unsafe(link_section = ".data.exported")]
        pub static $section: u32 = 0;
        #[cfg_attr(any(), no_mangle)]
        pub fn $other() {}
        #[export_name = $symbol]
        pub extern "C" fn symbol() {}
        #[cfg_attr($cfg, link_section = $place)]
        pub static CONFIGURED: u32 = 0;
    };
}

export!(exported, EXPORTED, other, "exported_symbol", ".data.configured", all());
