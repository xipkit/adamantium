#![allow(dead_code)]

use rand::random as draw;

fn rand_08() {
    use rand_08::{SeedableRng, rngs::StdRng};

    let _ = rand_08::random::<u64>(); // disallowed_methods
    let _ = rand_08::thread_rng(); // disallowed_methods
    let _ = StdRng::from_entropy(); // disallowed_methods
    let _ = StdRng::seed_from_u64(7);
}

#[allow(deprecated, reason = "exercise rand 0.9's compatibility alias")]
fn rand_09() {
    use rand_09::{RngCore, SeedableRng, rngs::StdRng};

    let _ = rand_09::random::<u64>(); // disallowed_methods
    let _ = rand_09::thread_rng(); // disallowed_methods
    rand_09::rng().fill_bytes(&mut [0; 32]); // disallowed_methods
    let _ = StdRng::from_os_rng(); // disallowed_methods
    drop(StdRng::try_from_os_rng()); // disallowed_methods
    let _ = StdRng::seed_from_u64(7);
}

fn rand_10() {
    let _ = draw::<u64>(); // disallowed_methods
    let mut rng = rand::rng(); // disallowed_methods
    let _ = rand::random_range(0..=250_u64); // disallowed_methods
    let _ = rand::random_iter::<u8>().next(); // disallowed_methods
    let _ = rand::random_bool(0.5); // disallowed_methods
    let _ = rand::random_ratio(1, 2); // disallowed_methods
    rand::fill(&mut [0_u8; 8]); // disallowed_methods
    let _ = rand::make_rng::<rand::rngs::StdRng>(); // disallowed_methods
    drop(rng.reseed()); // disallowed_methods
}

fn fastrand() {
    use fastrand::{Rng, u64 as jitter};

    let _ = jitter(0..=250); // disallowed_methods
    let _ = fastrand::bool(); // disallowed_methods
    let _ = fastrand::char('a'..='z'); // disallowed_methods
    let _ = fastrand::choice([1, 2]); // disallowed_methods
    fastrand::shuffle(&mut [1, 2]); // disallowed_methods
    fastrand::fill(&mut [0; 8]); // disallowed_methods
    fastrand::seed(7); // disallowed_methods
    let _ = fastrand::get_seed(); // disallowed_methods
    let _ = Rng::new(); // disallowed_methods
}

fn replayable_and_owned() {
    use rand::{Rng, SeedableRng, rngs::StdRng};

    let _ = adamantium::random::u64();
    let _ = adamantium::random::up_to(250);
    let mut rng = StdRng::seed_from_u64(7);
    let _ = rng.next_u64();
    let mut rng = fastrand::Rng::with_seed(7);
    rng.seed(11);
    let _ = rng.u64(0..=250);
}
