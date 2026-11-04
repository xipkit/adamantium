pub mod text;

/// Skipped: the rule is allowed everywhere.
pub fn first(values: &[i32]) -> i32 {
    values[0]
}

/// Reported.
pub fn only(value: Option<i32>) -> i32 {
    value.unwrap()
}

/// Reported: the exception covers src/text/, not this file.
pub fn head(text: &str) -> &str {
    &text[..1]
}
