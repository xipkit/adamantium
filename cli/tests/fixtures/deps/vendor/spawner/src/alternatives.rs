// Bindings of one name behind `cfg`s that can't be known: any of them may
// be the one built, so a call through the name counts for each.
#[cfg(spawner_custom)]
use std::mem::forget;
#[cfg(not(spawner_custom))]
use std::mem::drop as forget;

pub fn either_import(value: Box<u8>) {
    forget(value); // std::mem::forget
}

#[cfg(spawner_custom)]
mod imp {
    pub use std::mem::forget as release;
}

#[cfg(not(spawner_custom))]
mod imp {
    pub use std::mem::drop as release;
}

pub fn either_module(value: Box<u8>) {
    imp::release(value); // std::mem::forget
}

pub fn either_item() {
    #[cfg(spawner_custom)]
    fn spawn(_: ()) {}
    #[cfg(not(spawner_custom))]
    use rt::spawn;
    spawn(()); // tokio::spawn
}

// Known `cfg`s still pick one.
#[cfg(unix)]
use std::mem::drop as release;
#[cfg(not(unix))]
use std::mem::forget as release;

pub fn known(value: Box<u8>) {
    release(value);
}
