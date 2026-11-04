// Included from the root package through `#[path]`, so Clippy names it
// `src/../shared.rs`. The report should name it `shared.rs`.
pub fn shared(text: &str) -> &str {
    &text[..1]
}
