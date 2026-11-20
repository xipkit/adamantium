use tokio::time::{Instant, sleep_until};

pub async fn stored() {
    let timer = sleep_until(Instant::now());
    timer.await;
}

pub fn measure() -> std::time::Duration {
    let started = Instant::now();
    started.elapsed()
}

pub async fn reused() {
    let deadline = Instant::now();
    println!("{deadline:?}");
    ::adamantium::time::sleep_until(deadline).await;
}

pub async fn custom_offset() {
    struct Offset;
    impl std::ops::Add<Offset> for Instant {
        type Output = Instant;

        fn add(self, _: Offset) -> Self::Output {
            std::hint::black_box(self)
        }
    }
    sleep_until(Instant::now() + Offset).await;
}
