// A `use` resolves in the scope that declares it, not where its name is
// used, so a local module named like its first segment doesn't hide it.
use std::mem;
use mem::forget;

pub fn module_import(value: Box<u8>) {
    mod mem {}
    forget(value); // std::mem::forget
}

pub fn block_import(value: Box<u8>) {
    use std::mem as memory;
    use memory::forget as leak;
    {
        mod memory {}
        leak(value); // std::mem::forget
    }
}
