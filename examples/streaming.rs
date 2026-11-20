#[tokio::main]
async fn main() -> Result<(), adamantium::ScopeError<std::convert::Infallible>> {
    use adamantium::{Cancellation, concurrently_ordered, concurrently_unordered_until, time};
    use futures_util::StreamExt;
    use std::{convert::Infallible, time::Duration};

    let work = |n: u64| async move {
        time::sleep(Duration::from_millis((6 - n) * 10)).await;
        Ok::<_, Infallible>(n)
    };

    let ordered = concurrently_ordered(0..6, 3, work);
    let mut ordered = std::pin::pin!(ordered);
    while let Some(value) = ordered.next().await {
        println!("ordered: {}", value?);
        time::sleep(Duration::from_millis(30)).await;
    }

    let cancellation = Cancellation::new();
    let unordered = concurrently_unordered_until(0..6, 3, cancellation.clone(), work);
    let mut unordered = std::pin::pin!(unordered);
    for _ in 0..3 {
        if let Some(value) = unordered.next().await {
            println!("completion order: {}", value?);
        }
    }
    cancellation.cancel();
    while let Some(value) = unordered.next().await {
        match value {
            Err(adamantium::ScopeError::Cancelled) => {
                println!("cancelled; outstanding tasks drained")
            }
            value => println!("completion order: {}", value?),
        }
    }
    Ok(())
}
