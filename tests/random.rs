use adamantium::random;

#[test]
fn draws_up_to_a_bound_include_both_ends_and_nothing_past_them() {
    assert_eq!(random::up_to(0), 0);
    let mut seen = [false; 4];
    for _ in 0..1_000 {
        let draw = random::up_to(3);
        assert!(draw <= 3);
        seen[usize::try_from(draw).unwrap()] = true;
    }
    assert_eq!(seen, [true; 4]);
    // The largest bound covers every value, so it can draw any of them.
    let mut high = false;
    for _ in 0..64 {
        high |= random::up_to(u64::MAX) > u64::MAX / 2;
    }
    assert!(high);
}

#[test]
fn successive_draws_differ() {
    let mut draws = (0..1_000).map(|_| random::u64()).collect::<Vec<_>>();
    draws.sort_unstable();
    draws.dedup();
    assert_eq!(draws.len(), 1_000);
}
