#[uniffi::export]
async fn nested() -> impl std::future::Future<Output = u32> { async { 1 } }
uniffi::setup_scaffolding!();
fn main() {}
