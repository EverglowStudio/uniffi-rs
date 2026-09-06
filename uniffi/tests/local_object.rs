#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]

use std::{cell::Cell, rc::Rc, sync::Arc};
use uniffi::{
    async_runtime::{LocalExecutor, LocalObject},
    FfiConverter,
};

#[derive(uniffi::Object)]
#[uniffi(local)]
pub struct Counter {
    value: Rc<Cell<u32>>,
}

#[uniffi::export(local)]
impl Counter {
    pub fn get(&self) -> u32 {
        self.value.get()
    }
    pub async fn add(&self, label: &str) -> u32 {
        std::future::ready(()).await;
        self.value.set(self.value.get() + label.len() as u32);
        self.value.get()
    }
}

uniffi::setup_scaffolding!();

#[test]
fn generated_local_helpers_and_ffi_handles_transfer_only_the_proxy() {
    type Proxy = Arc<LocalObject<Counter>>;
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Proxy>();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let owner = LocalExecutor::new(1).unwrap();
            let proxy = Arc::new(
                owner
                    .create(|| async {
                        Counter {
                            value: Rc::new(Cell::new(10)),
                        }
                    })
                    .await
                    .unwrap(),
            );
            let handle = <Proxy as FfiConverter<UniFfiTag>>::lower(proxy);
            // Generated clone/free use this same exact proxy type, never Counter.
            let cloned_handle = unsafe { handle.clone_arc_handle::<LocalObject<Counter>>() };
            let lifted = <Proxy as FfiConverter<UniFfiTag>>::try_lift(handle).unwrap();
            let cloned = <Proxy as FfiConverter<UniFfiTag>>::try_lift(cloned_handle).unwrap();
            assert!(Arc::ptr_eq(&lifted, &cloned));
            assert_eq!(
                Counter::__uniffi_local_get(lifted.clone()).await.unwrap(),
                10
            );
            assert_eq!(
                Counter::__uniffi_local_add(cloned.clone(), "abc".into())
                    .await
                    .unwrap(),
                13
            );

            let mut bytes = Vec::new();
            <Proxy as FfiConverter<UniFfiTag>>::write(cloned, &mut bytes);
            let read = <Proxy as FfiConverter<UniFfiTag>>::try_read(&mut bytes.as_slice()).unwrap();
            assert!(Arc::ptr_eq(&lifted, &read));
            assert!(<Proxy as FfiConverter<UniFfiTag>>::try_read(&mut &b"short"[..]).is_err());
            Counter::__uniffi_local_close(read).await.unwrap();
            assert_eq!(
                Counter::__uniffi_local_get(lifted).await,
                Err(uniffi::async_runtime::LocalError::Closed)
            );
            owner.close().await.unwrap();
        });
}
