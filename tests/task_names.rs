#![cfg(feature = "tokio")]

use adamantium::Tasks;

#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
#[tokio::test]
async fn named_tasks_preserve_values_and_error_handling() {
    let mut tasks = Tasks::<&str>::new();
    let failed = tasks.spawn_named("handled", async { Err::<(), _>("handled") });
    assert_eq!(tasks.value(failed).await, Err("handled"));
    let value = tasks.spawn_named(String::from("value"), async { Ok(42) });
    assert_eq!(tasks.value(value).await, Ok(42));
    let blocking = tasks.spawn_blocking_named("blocking", || Ok(7));
    assert_eq!(tasks.value(blocking).await, Ok(7));
    tasks.spawn_blocking_named("unclaimed", || Err::<(), _>("unclaimed"));
    assert_eq!(tasks.join(Ok(())).await, Err("unclaimed"));
}

#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
#[cfg(not(feature = "tracing"))]
#[tokio::test]
async fn names_are_not_converted_without_tracing() {
    struct Unconverted;

    impl From<Unconverted> for adamantium::Str {
        fn from(_: Unconverted) -> Self {
            panic!("a disabled task name must never be converted")
        }
    }

    let mut tasks = Tasks::<&str>::new();
    let value = tasks.spawn_named(Unconverted, async { Ok(42) });
    assert_eq!(tasks.value(value).await, Ok(42));
    let value = tasks.spawn_blocking_named(Unconverted, || Ok(7));
    assert_eq!(tasks.value(value).await, Ok(7));
    assert_eq!(tasks.join(Ok(())).await, Ok(()));
}
