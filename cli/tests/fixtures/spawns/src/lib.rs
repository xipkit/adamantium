use std::{future::ready, io};

use async_std::task::{Builder, spawn as async_std_spawn};
use smol::{Executor, LocalExecutor, spawn as smol_spawn};

/// Starts tasks through async-std's free functions and an imported alias.
pub fn async_std_spawns() {
    drop(async_std::task::spawn(async {})); // disallowed
    drop(async_std_spawn(async {})); // disallowed
    drop(async_std::task::spawn_local(async {})); // disallowed
    drop(async_std::task::spawn_blocking(|| {})); // disallowed
}

/// Starts tasks through async-std's builder using method and path syntax.
pub fn async_std_builder_spawns() -> io::Result<()> {
    drop(Builder::new().spawn(async {})?); // disallowed
    drop(Builder::spawn(Builder::new(), async {})?); // disallowed
    drop(Builder::new().local(async {})?); // disallowed
    drop(Builder::local(Builder::new(), async {})?); // disallowed
    Ok(())
}

/// Starts tasks through smol's free functions and an imported alias.
pub fn smol_spawns() {
    drop(smol::spawn(async {})); // disallowed
    drop(smol_spawn(async {})); // disallowed
    drop(smol::unblock(|| {})); // disallowed
}

/// Starts tasks through smol's executor using method and path syntax.
pub fn smol_executor_spawns(executor: &Executor<'_>) {
    drop(executor.spawn(async {})); // disallowed
    drop(Executor::spawn(executor, async {})); // disallowed
    let mut handles = Vec::new();
    executor.spawn_many([ready(())], &mut handles); // disallowed
    Executor::spawn_many(executor, [ready(())], &mut handles); // disallowed
}

/// Starts tasks through smol's local executor using method and path syntax.
pub fn smol_local_executor_spawns(executor: &LocalExecutor<'_>) {
    drop(executor.spawn(async {})); // disallowed
    drop(LocalExecutor::spawn(executor, async {})); // disallowed
    let mut handles = Vec::new();
    executor.spawn_many([ready(())], &mut handles); // disallowed
    LocalExecutor::spawn_many(executor, [ready(())], &mut handles); // disallowed
}

/// Enters each runtime without starting tasks that can outlive this call.
pub fn runtime_entry_points() {
    async_std::task::block_on(async {});
    Builder::new().blocking(async {});
    smol::block_on(async {});
    smol::block_on(Executor::new().run(async {}));
    smol::block_on(LocalExecutor::new().run(async {}));
}
