#[derive(uniffi::Object)]
#[uniffi(local)]
struct Counter;

#[uniffi::export(local)]
impl Counter {
    pub fn change(&mut self) {}
}

uniffi::setup_scaffolding!();
fn main() {}
