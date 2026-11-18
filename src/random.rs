//! Random draws that the deterministic harness records and replays.
//!
//! Draw from [`u64()`] or [`up_to`] wherever an application would otherwise
//! reach for a random number generator, such as to spread out retries or to
//! pick a peer. The draws are not suitable for keys, tokens or anything else
//! that must stay secret.
//!
//! Outside `adamantium::testing` (with the `deterministic` feature), each draw
//! comes from the standard library's per-process random hashing keys. Inside an
//! execution, each draw comes from the scheduler instead: a seed gives the same
//! draws in the same order, an `adamantium::testing::Replay` repeats them
//! exactly, and a replay whose code draws a different number of times fails as
//! a mismatch.
//!
//! ```
//! use adamantium::random;
//!
//! let die = random::up_to(5) + 1;
//! assert!((1..=6).contains(&die));
//! ```

use std::hash::{BuildHasher, Hasher, RandomState};

/// Returns a uniformly distributed random number.
pub fn u64() -> u64 {
    #[cfg(feature = "deterministic")]
    if crate::runtime::World::current().is_model() {
        return shuttle_engine::runtime::execution::ExecutionState::next_u64();
    }
    // Every RandomState has keys that differ from those of the last one, so
    // the hash of an empty input under fresh keys is a new draw each time.
    RandomState::new().build_hasher().finish()
}

/// Returns a uniformly distributed random number from zero to `max`, both
/// included.
pub fn up_to(max: u64) -> u64 {
    let Some(bound) = max.checked_add(1) else {
        return u64();
    };
    // Draws below `threshold` would make the low remainders more likely than
    // the high ones, so they are drawn again. Fewer than half of all draws
    // fall below it, whatever the bound.
    let threshold = bound.wrapping_neg() % bound;
    loop {
        let draw = u64();
        if draw >= threshold {
            return draw % bound;
        }
    }
}
