// The local flavor expands to `LocalRuntime::block_on`.
#[tokio::main(flavor = "local")] // disallowed
async fn main() {}
