use std::{convert::Infallible, future::ready, time::Duration};

use adamantium::{Cancellation, Channel, ReceiveError, ScopeError, SendError, Tasks, time};
use tokio::time::timeout as deadline;

pub async fn timeouts() -> Result<u8, tokio::time::error::Elapsed> {
    let first = tokio::time::timeout(Duration::from_secs(1), ready(1)).await?; // disallowed_methods
    let second = deadline(Duration::from_secs(1), ready(2)).await?; // disallowed_methods
    Ok(first + second)
}

pub async fn scoped_deadline() -> Result<u8, ScopeError<Infallible>> {
    Tasks::run_until(
        Cancellation::new(),
        Some(time::now() + Duration::from_secs(1)),
        async |tasks| {
            let task = tasks.spawn(async { Ok(1) });
            tasks.value(task).await
        },
    )
    .await
}

pub async fn receive_deadline() -> Result<u8, ReceiveError> {
    Channel::<u8>::bounded(1)
        .receive_until(
            &Cancellation::new(),
            Some(time::now() + Duration::from_secs(1)),
        )
        .await
}

pub async fn send_deadline() -> Result<(), SendError<u8>> {
    Channel::bounded(1)
        .send_until(
            &Cancellation::new(),
            Some(time::now() + Duration::from_secs(1)),
            1,
        )
        .await
}

#[allow(clippy::disallowed_methods, reason = "the fixture's function-scoped exemption")]
pub async fn function_exception() -> Result<u8, tokio::time::error::Elapsed> {
    deadline(Duration::from_secs(1), ready(1)).await
}

pub async fn call_exception() -> Result<u8, tokio::time::error::Elapsed> {
    #[allow(clippy::disallowed_methods, reason = "the fixture's call-scoped exemption")]
    let result = tokio::time::timeout(Duration::from_secs(1), ready(1)).await;
    result
}
