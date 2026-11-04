// A helper is not a test to Clippy, so this unwrap would count. The skip list
// leaves the whole directory out.
fn helper(value: Option<i32>) -> i32 {
    value.unwrap()
}

#[test]
fn uses_helper() {
    assert_eq!(helper(Some(1)), 1);
}
