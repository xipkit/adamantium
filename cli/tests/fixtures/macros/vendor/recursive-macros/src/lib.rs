#[macro_export]
macro_rules! sleep {
    (@end) => { std::thread::sleep(std::time::Duration::ZERO) };
    (@next) => { $crate::sleep!(@end) };
    () => { $crate::sleep!(@next) };
}
