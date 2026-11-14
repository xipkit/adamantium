use tokio::runtime;

runtime! {
    yes {
        mod fast;
    }
    no {
        mod slow;
    }
}
