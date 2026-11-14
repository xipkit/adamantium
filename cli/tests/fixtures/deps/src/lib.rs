// The workspace's own code is for `check`, not `deps`.
pub fn own() {
    unsafe {}
    std::thread::spawn(|| ());
}
