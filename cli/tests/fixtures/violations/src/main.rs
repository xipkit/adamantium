// A binary target that depends on the library above it. It has one finding
// of its own, which only shows up when the library's findings do not stop
// the build.
fn main() {
    let missing: Option<i32> = None;
    let _ = missing.unwrap();
}
