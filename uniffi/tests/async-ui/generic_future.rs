#[uniffi::export]
fn generic<T: Default + 'static>() -> impl std::future::Future<Output = T> {
    async { T::default() }
}
uniffi::setup_scaffolding!();
fn main() {}
