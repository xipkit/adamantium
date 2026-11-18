//! Traverse a small graph whose children discover more work in one scope.

use adamantium::{Actor, SpawnError, Spawner, Tasks};
use std::{collections::BTreeSet, future::Future, pin::Pin};

fn visit(
    page: &'static str,
    spawner: Spawner<SpawnError>,
    visited: Actor<BTreeSet<&'static str>>,
) -> Pin<Box<dyn Future<Output = Result<(), SpawnError>> + Send>> {
    Box::pin(async move {
        if !visited.with(|seen| seen.insert(page)) {
            return Ok(());
        }
        let links: &[&str] = match page {
            "home" => &["about", "projects"],
            "about" => &["home", "contact"],
            "projects" => &["contact", "docs"],
            _ => &[],
        };
        for &link in links {
            spawner.spawn(visit(link, spawner.clone(), visited.clone()))?;
        }
        Ok(())
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), SpawnError> {
    let visited = Actor::new(BTreeSet::new());
    Tasks::run(async |tasks| {
        tasks.spawn(visit("home", tasks.spawner(), visited.clone()));
        Ok(())
    })
    .await?;
    let pages = visited.snapshot();
    assert_eq!(
        pages,
        BTreeSet::from(["about", "contact", "docs", "home", "projects"])
    );
    println!("Joined all {} discovered pages: {pages:?}", pages.len());
    Ok(())
}
