#[uniffi::export]
fn nested() -> impl uniffi::deps::futures_core::Stream<Item = uniffi::UniFfiStream<u32, String>> {
    uniffi::deps::futures_util::stream::empty()
}
uniffi::setup_scaffolding!();
fn main() {}
