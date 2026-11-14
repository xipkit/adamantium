pub fn add(left: u32, right: u32) -> u32 {
    left + right // strict
}

pub fn divide(left: u32, right: u32) -> u32 {
    left / right // strict
}

pub fn negate(value: i32) -> i32 {
    -value // strict
}

pub fn checked_add(left: u32, right: u32) -> Option<u32> {
    left.checked_add(right)
}

pub fn checked_divide(left: u32, right: u32) -> Option<u32> {
    left.checked_div(right)
}

pub fn wrapping_negate(value: i32) -> i32 {
    value.wrapping_neg()
}

pub fn saturating_add(left: u32, right: u32) -> u32 {
    left.saturating_add(right)
}
