// Declares foreign functions and nothing else.

// Counted: `forbid(unsafe_code)` rejects it, and a `safe fn` in it is
// called without an `unsafe` block.
unsafe extern "C" {
    pub safe fn abs(value: i32) -> i32;
}

// Not counted: before the 2024 edition, an `extern` block needn't be
// `unsafe`, and its functions are called only in `unsafe` blocks, which
// are counted where they are.
extern "C" {
    pub fn labs(value: i64) -> i64;
}

#[cfg(any())]
unsafe extern "C" {
    pub fn llabs(value: i64) -> i64;
}
