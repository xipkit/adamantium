use std::time::Duration;

use adamantium::{Cancellation, Tasks};
use tokio::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .try_init()?;

    let result = Tasks::<std::io::Error>::run_until(
        Cancellation::new(),
        Some(Instant::now() + Duration::from_millis(100)),
        async |tasks| {
            let completed = tasks.spawn_named("load profile", async { Ok(42) });
            tasks.spawn_named("load feed", async {
                tracing::info!("waiting for feed");
                std::future::pending::<Result<(), std::io::Error>>().await
            });
            tasks.value(completed).await
        },
    )
    .await;
    println!("{result:?}");
    Ok(())
}
