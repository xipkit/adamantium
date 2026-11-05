use std::io;

use tokio::{runtime::Handle, task::Builder};

pub fn spawn() -> io::Result<()> {
    drop(Builder::new().spawn(async {})?); // disallowed
    drop(Builder::spawn(Builder::new(), async {})?); // disallowed
    Ok(())
}

pub fn spawn_on(handle: &Handle) -> io::Result<()> {
    drop(Builder::new().spawn_on(async {}, handle)?); // disallowed
    drop(Builder::spawn_on(Builder::new(), async {}, handle)?); // disallowed
    Ok(())
}

pub fn spawn_local() -> io::Result<()> {
    drop(Builder::new().spawn_local(async {})?); // disallowed
    drop(Builder::spawn_local(Builder::new(), async {})?); // disallowed
    Ok(())
}

pub fn spawn_local_on(set: &tokio::task::LocalSet) -> io::Result<()> {
    drop(Builder::new().spawn_local_on(async {}, set)?); // disallowed
    drop(Builder::spawn_local_on(Builder::new(), async {}, set)?); // disallowed
    Ok(())
}

pub fn spawn_blocking() -> io::Result<()> {
    drop(Builder::new().spawn_blocking(|| {})?); // disallowed
    drop(Builder::spawn_blocking(Builder::new(), || {})?); // disallowed
    Ok(())
}

pub fn spawn_blocking_on(handle: &Handle) -> io::Result<()> {
    drop(Builder::new().spawn_blocking_on(|| {}, handle)?); // disallowed
    drop(Builder::spawn_blocking_on(Builder::new(), || {}, handle)?); // disallowed
    Ok(())
}
