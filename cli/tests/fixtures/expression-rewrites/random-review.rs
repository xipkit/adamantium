pub fn distributions() -> (bool, i32, u64, u64, u64) {
    (
        rand::random::<bool>(),
        rand::random_range(0..=10),
        rand::random(),
        fastrand::u64(0..10),
        fastrand::u64(1..=10),
    )
}

pub fn shadowed_type() {
    #[allow(non_camel_case_types)]
    type u64 = u32;
    let _ = rand::random::<u64>();
}
