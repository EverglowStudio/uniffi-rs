#[derive(uniffi::Record)]
struct Invalid { future: uniffi::async_runtime::BoxFuture<'static, u32> }
uniffi::setup_scaffolding!();
fn main() {}
