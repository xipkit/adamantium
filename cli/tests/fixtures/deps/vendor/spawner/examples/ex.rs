// Not compiled into the library, so not counted.
pub fn uncounted() {
    unsafe {}
    rt::spawn(());
}
