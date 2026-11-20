use fastrand::u64 as fast_draw;
use rand::{random as draw, random_range as range};

fn main() {
    let _: u64 = draw::<u64>();
    let _: u64 = rand_08::random::<u64>();
    let _: u64 = fast_draw(..,);
    let limit = 7;
    let bounded = fast_draw(0 /* lower bound */ ..=limit);
    let other = range(0..=7_u64);
    let typed = range::<u64, _>(0..=limit);
    println!("{}", bounded <= limit && other <= limit && typed <= limit);
}
