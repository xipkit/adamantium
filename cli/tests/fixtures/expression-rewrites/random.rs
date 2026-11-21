use std::time::Duration;

use fastrand::u64 as fast_draw;
use rand::{random as draw, random_range as range};

fn main() {
    let full = Duration::from_nanos(draw::<u64>());
    let whole = std::time::Duration::new(rand_08::random(), 0);
    let _: u64 = fast_draw(..,);
    let limit = 7;
    let bounded = fast_draw(0 /* lower bound */ ..=limit);
    let other = Duration::from_millis(range(0..=7));
    let typed = Duration::from_secs((range::<u64, _>(0..=limit)));
    println!(
        "{}",
        full.subsec_nanos() < 1_000_000_000
            && whole.subsec_nanos() == 0
            && bounded <= limit
            && other <= Duration::from_millis(limit)
            && typed <= Duration::from_secs(limit)
    );
}
