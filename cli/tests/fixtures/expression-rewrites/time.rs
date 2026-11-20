use std::time::Duration;
use tokio::time::{Instant as Clock, sleep_until as wait};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    wait(Clock::now()).await;
    let deadline = Clock::now() + Duration::from_millis(1);
    wait(deadline).await;
    adamantium::time::sleep_until(Clock::now()).await;
    println!("timers completed");
}
