#[macro_export]
macro_rules! select {
    ($($arm:ident { $($body:tt)* })*) => {
        $crate::arm! { $($($body)*)* }
    };
}

#[macro_export]
macro_rules! arm {
    ($($body:tt)*) => { $($body)* };
}
