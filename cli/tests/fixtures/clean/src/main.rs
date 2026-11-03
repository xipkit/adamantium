//! Fetches a few "pages" concurrently, tallies them through an actor, and
//! streams them through a channel: the shape of a program in the fragment.

use std::time::Duration;

use adamantium::{Actor, Array, Channel, Map, Str, Tasks, blocking, concurrently};

#[derive(Default)]
struct Tally {
    bytes_by_host: Map<Str, u64>,
}

impl Tally {
    fn record(&mut self, host: &Str, bytes: u64) {
        let total = self.bytes_by_host.get(host).copied().unwrap_or(0) + bytes;
        self.bytes_by_host.insert(host.clone(), total);
    }

    fn total(&self) -> u64 {
        self.bytes_by_host.values().sum()
    }
}

async fn fetch(url: Str) -> Result<Str, Str> {
    tokio::time::sleep(Duration::from_millis(5)).await;
    if url.contains("broken") {
        return Err(Str::from("connection refused"));
    }
    Ok(Str::from("<html>...</html>"))
}

fn host(url: &Str) -> Str {
    url.split('/').nth(2).unwrap_or("").into()
}

#[tokio::main]
async fn main() -> Result<(), Str> {
    let urls = Array::from([
        Str::from("https://a.example/1"),
        Str::from("https://a.example/2"),
        Str::from("https://b.example/1"),
    ]);
    let tally = Actor::new(Tally::default());

    let pages = concurrently(urls.iter().cloned(), 2, fetch).await?;
    for (url, page) in urls.iter().zip(&pages) {
        let bytes = u64::try_from(page.len()).unwrap_or(u64::MAX);
        tally.with(|tally| tally.record(&host(url), bytes));
    }

    let feed = Channel::<Str>::bounded(2);
    Tasks::<Str>::run(async |t| {
        let producer = feed.clone();
        let urls = urls.clone();
        t.spawn(async move {
            for url in &urls {
                producer.send(url.clone()).await.map_err(|_| Str::from("closed"))?;
            }
            producer.close();
            Ok(())
        });
        let consumer = feed.clone();
        let tally = tally.clone();
        t.spawn(async move {
            while let Some(url) = consumer.receive().await {
                let page = fetch(url.clone()).await?;
                let bytes = u64::try_from(page.len()).unwrap_or(u64::MAX);
                tally.with(|tally| tally.record(&host(&url), bytes));
            }
            Ok(())
        });
        Ok(())
    })
    .await?;

    let digest = blocking(move || format!("{} bytes", tally.read(Tally::total))).await;
    println!("{digest}");
    Ok(())
}
