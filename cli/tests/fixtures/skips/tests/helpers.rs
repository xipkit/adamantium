// Clippy applies the cast rules to tests as well, so this cast would count.
// The skip list leaves the whole directory out.
fn helper(value: u64) -> u8 {
    value as u8
}

#[test]
fn uses_helper() {
    assert_eq!(helper(1), 1);
}
