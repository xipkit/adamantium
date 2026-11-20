use std::hash::{BuildHasher, Hasher, RandomState};

/// Returns a uniform random `u64` for non-cryptographic use.
///
/// The deterministic harness records and replays draws in order.
pub fn u64() -> u64 {
    #[cfg(feature = "deterministic")]
    if crate::runtime::World::current().is_model() {
        return shuttle_engine::runtime::execution::ExecutionState::next_u64();
    }
    RandomState::new().build_hasher().finish()
}

/// Returns a uniform random number in `0..=max` for non-cryptographic use.
///
/// ```
/// use adamantium::random;
///
/// let die = random::up_to(5) + 1;
/// assert!((1..=6).contains(&die));
/// ```
pub fn up_to(max: u64) -> u64 {
    let Some(bound) = max.checked_add(1) else {
        return u64();
    };
    let threshold = bound.wrapping_neg() % bound;
    loop {
        let draw = u64();
        if draw >= threshold {
            return draw % bound;
        }
    }
}
