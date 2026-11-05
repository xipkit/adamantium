use tokio::runtime::{Handle, LocalRuntime, Runtime};

pub fn handle_spawn(handle: &Handle) {
    drop(handle.spawn(async {})); // disallowed
    drop(Handle::spawn(handle, async {})); // disallowed
}

pub fn handle_spawn_blocking(handle: &Handle) {
    drop(handle.spawn_blocking(|| {})); // disallowed
    drop(Handle::spawn_blocking(handle, || {})); // disallowed
}

pub fn handle_block_on(handle: &Handle) {
    handle.block_on(async {}); // disallowed
    Handle::block_on(handle, async {}); // disallowed
}

pub fn runtime_spawn(runtime: &Runtime) {
    drop(runtime.spawn(async {})); // disallowed
    drop(Runtime::spawn(runtime, async {})); // disallowed
}

pub fn runtime_spawn_blocking(runtime: &Runtime) {
    drop(runtime.spawn_blocking(|| {})); // disallowed
    drop(Runtime::spawn_blocking(runtime, || {})); // disallowed
}

/// Allowed: the way into the runtime for a program without `#[tokio::main]`.
pub fn runtime_block_on(runtime: &Runtime) {
    runtime.block_on(async {});
    Runtime::block_on(runtime, async {});
}

pub fn local_runtime_spawn_local(runtime: &LocalRuntime) {
    drop(runtime.spawn_local(async {})); // disallowed
    drop(LocalRuntime::spawn_local(runtime, async {})); // disallowed
}

pub fn local_runtime_spawn_blocking(runtime: &LocalRuntime) {
    drop(runtime.spawn_blocking(|| {})); // disallowed
    drop(LocalRuntime::spawn_blocking(runtime, || {})); // disallowed
}

pub fn local_runtime_block_on(runtime: &LocalRuntime) {
    runtime.block_on(async {}); // disallowed
    LocalRuntime::block_on(runtime, async {}); // disallowed
}

#[cfg(test)]
mod tests {
    // Allowed: the attribute expands to `Runtime::block_on`.
    #[tokio::test]
    async fn enters_the_runtime() {}
}
