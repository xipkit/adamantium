use std::{
    convert::Infallible,
    future::{poll_fn, ready},
    task::Poll,
    time::Duration,
};

use futures::{FutureExt, select as futures_pick};
use tokio::select as tokio_pick;

pub async fn tokio_direct() -> u8 {
    tokio::select! { value = ready(1) => value } // disallowed_methods
}

pub async fn tokio_qualified() -> u8 {
    ::tokio::select! { value = ready(1) => value } // disallowed_methods
}

pub async fn tokio_aliased() -> u8 {
    tokio_pick! { value = ready(1) => value } // disallowed_methods
}

pub async fn tokio_biased() -> u8 {
    tokio::select! { biased; value = ready(1) => value } // disallowed_methods
}

pub async fn tokio_with_else() -> u8 {
    tokio::select! { value = ready(1), if false => value, else => 0 } // disallowed_methods
}

pub fn tokio_else_only() -> u8 {
    tokio::select! { else => 0 }
}

pub async fn futures_direct() -> u8 {
    futures::select! { value = ready(1).fuse() => value } // disallowed_macros
}

pub async fn futures_aliased() -> u8 {
    futures_pick! { value = ready(1).fuse() => value } // disallowed_macros
}

#[expect(
    clippy::disallowed_methods,
    reason = "exercise a function-scoped exception"
)]
pub async fn tokio_function_exception() -> u8 {
    tokio_pick! { value = ready(1) => value }
}

#[expect(
    clippy::disallowed_macros,
    reason = "exercise a function-scoped exception"
)]
pub async fn futures_function_exception() -> u8 {
    futures_pick! { value = ready(1).fuse() => value }
}

#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    reason = "exercise a module-scoped exception"
)]
pub mod module_exception {
    use super::*;

    pub async fn allowed() -> (u8, u8) {
        let a = tokio_pick! { value = ready(1) => value };
        let b = futures_pick! { value = ready(1).fuse() => value };
        (a, b)
    }
}

pub async fn statement_exceptions() -> (u8, u8) {
    #[expect(
        clippy::disallowed_methods,
        reason = "exercise a statement-scoped exception"
    )]
    let a = tokio_pick! { value = ready(1) => value };
    #[expect(
        clippy::disallowed_macros,
        reason = "exercise a statement-scoped exception"
    )]
    let b = futures_pick! { value = ready(1).fuse() => value };
    tokio::time::sleep(Duration::ZERO).await; // disallowed_methods
    (a, b)
}

pub async fn unrelated() -> (u8, u8, u8) {
    let (a, b) = tokio::join!(ready(1), ready(2));
    (a, b, poll_fn(|_| Poll::Ready(3)).await)
}

pub async fn first_success() -> Result<u8, Vec<Infallible>> {
    adamantium::first_ok([1, 2], 2, |value| async move { Ok(value) }).await
}
