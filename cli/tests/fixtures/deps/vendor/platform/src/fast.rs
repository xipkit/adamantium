// A `cfg_if!` in a module's file, with another inside it.
cfg_if::cfg_if! {
    if #[cfg(target_pointer_width = "8")] {
        mod tiny;
    } else {
        cfg_if::cfg_if! {
            if #[cfg(platform_custom)] {
                pub unsafe fn custom() {}
            } else {
                pub fn portable() {
                    unsafe {}
                }
            }
        }
    }
}
