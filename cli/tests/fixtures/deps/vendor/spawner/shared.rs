// Beside src/, reached through `#[path = "../shared.rs"]`.
pub fn shared() {
    rt::spawn(()); // tokio::spawn
}
