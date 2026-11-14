cfg_select! {
    target_pointer_width = "8" => { mod missing_nested; }
    _ => {
        use std::thread::sleep as pause;

        pub unsafe fn selected() {}

        pub fn wait() {
            pause(std::time::Duration::ZERO); // std::thread::sleep
        }
    }
}
