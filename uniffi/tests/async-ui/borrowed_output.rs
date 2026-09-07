#[uniffi::export]
async fn borrowed(value: &str) -> &str { value }
uniffi::setup_scaffolding!();
fn main() {}
