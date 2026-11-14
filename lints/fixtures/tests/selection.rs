#[allow(dead_code)]
async fn test_target() {
    let _ = std::fs::read("missing"); // blocking
}
