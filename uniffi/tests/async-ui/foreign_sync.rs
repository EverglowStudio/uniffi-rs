#[uniffi::export(foreign)]
pub trait SyncFuture: Send + Sync {
    fn value(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = u32> + Send + Sync + 'static>>;
}
uniffi::setup_scaffolding!();
fn main() {}
