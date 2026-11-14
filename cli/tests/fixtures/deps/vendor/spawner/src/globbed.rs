use rt::task::*;

pub fn globbed() {
    spawn(()); // unresolved
}

mod own {
    pub struct Box;

    impl Box {
        pub fn leak() {}
    }
}

pub mod prelude_shadowed {
    use super::own::*;

    pub fn leak() {
        Box::leak(); // unresolved
    }
}
