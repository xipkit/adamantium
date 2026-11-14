pub fn first() {
    recursive_macros::sleep!();
}
pub fn second() {
    recursive_macros::sleep!();
}
pub fn ordinary(value: Option<u8>) -> u8 {
    value.unwrap()
}
pub mod skipped;
