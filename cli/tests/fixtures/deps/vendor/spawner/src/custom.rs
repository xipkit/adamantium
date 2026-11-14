// Behind a cfg that can't be known, so counted.
pub fn custom() {
    rt::spawn(()); // tokio::spawn
}
