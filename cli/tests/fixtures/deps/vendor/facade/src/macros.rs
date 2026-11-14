macro_rules! root {
    () => {
        mod engine;

        pub struct Handle;

        unsafe impl Send for Handle {}

        pub fn start() {
            std::thread::spawn(|| ()); // std::thread::spawn
        }
    };
}
