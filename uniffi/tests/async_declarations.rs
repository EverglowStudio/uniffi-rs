#![cfg(feature = "async-runtime")]

use std::{
    cell::Cell,
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};
use uniffi::{
    async_runtime::{BoxFuture, LocalExecutor, LocalObject},
    deps::{
        futures_core::Stream,
        futures_util::{stream, StreamExt},
    },
};

#[derive(Debug, PartialEq, uniffi::Error)]
pub enum Failure {
    Factory,
    Item,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Failure {}
impl From<uniffi::UnexpectedUniFFICallbackError> for Failure {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Item
    }
}

#[uniffi::export]
pub fn future_value(value: u32) -> impl Future<Output = u32> + Send {
    async move { value + 1 }
}

#[uniffi::export]
pub fn boxed_value(value: u32) -> BoxFuture<'static, Result<u32, Failure>> {
    Box::pin(async move { Ok(value + 2) })
}

#[uniffi::export]
pub fn optional_values() -> impl Stream<Item = Option<u32>> + Send + 'static {
    stream::iter([None, Some(7)])
}

#[uniffi::export]
pub async fn make_values(
    fail: bool,
) -> Result<impl Stream<Item = Result<u32, Failure>> + Send + 'static, Failure> {
    if fail {
        Err(Failure::Factory)
    } else {
        Ok(stream::iter([Ok(3), Err(Failure::Item), Ok(99)]))
    }
}

#[derive(uniffi::Object)]
pub struct Service;
#[uniffi::export]
impl Service {
    pub fn boxed(&self, value: u32) -> BoxFuture<'_, u32> {
        Box::pin(async move { value + 3 })
    }
    pub fn values(&self) -> impl Stream<Item = u32> + Send + 'static {
        stream::iter([4, 5])
    }
    pub async fn values_later(&self) -> impl Stream<Item = u32> + Send + 'static {
        stream::iter([6])
    }
    pub async fn consume(
        &self,
        mut values: uniffi::UniFfiInputStream<u32, Failure>,
    ) -> Result<u32, Failure> {
        let mut sum = 0;
        while let Some(item) = values.next().await {
            sum += item?;
        }
        Ok(sum)
    }
}

#[uniffi::export(with_foreign)]
pub trait Provider: Send + Sync {
    fn value(&self, seed: u32) -> impl Future<Output = u32> + Send + '_;
    fn values(&self) -> impl Stream<Item = u32> + Send + 'static;
    async fn values_later(&self) -> Result<uniffi::UniFfiStream<u32, Failure>, Failure>;
    fn boxed_values(&self) -> BoxFuture<'_, uniffi::UniFfiStream<u32, Failure>>;
    fn consume(
        &self,
        values: uniffi::UniFfiInputStream<u32, Failure>,
    ) -> BoxFuture<'_, Result<u32, Failure>>;
}

// Compiling the generated callback implementations checks that internal local
// variables cannot shadow parameters from the original business signature.
#[uniffi::export(with_foreign)]
pub trait HygienicCallback: Send + Sync {
    fn convert(&self, value: String, status: u32, vtable: bool) -> String;
    fn boxed(&self, uniffi_receiver: String) -> BoxFuture<'_, String>;
    async fn receive(
        &self,
        uniffi_result: uniffi::UniFfiInputStream<u32, Failure>,
        callback: String,
        callback_data: u32,
        dropped: bool,
    ) -> u32;
}

#[derive(uniffi::Object)]
pub struct RustProvider;
#[uniffi::export]
impl Provider for RustProvider {
    fn value(&self, seed: u32) -> impl Future<Output = u32> + Send + '_ {
        async move { seed + 4 }
    }
    fn values(&self) -> impl Stream<Item = u32> + Send + 'static {
        stream::iter([8, 9])
    }
    async fn values_later(&self) -> Result<uniffi::UniFfiStream<u32, Failure>, Failure> {
        Ok(Box::pin(stream::iter([Ok(10)])))
    }
    fn boxed_values(&self) -> BoxFuture<'_, uniffi::UniFfiStream<u32, Failure>> {
        Box::pin(async {
            let values: uniffi::UniFfiStream<u32, Failure> = Box::pin(stream::iter([Ok(11)]));
            values
        })
    }
    fn consume(
        &self,
        values: uniffi::UniFfiInputStream<u32, Failure>,
    ) -> BoxFuture<'_, Result<u32, Failure>> {
        Box::pin(async move { Service.consume(values).await })
    }
}

#[derive(uniffi::Object)]
#[uniffi(local)]
pub struct LocalCounter {
    state: Rc<Cell<u32>>,
}
trait LocalBusiness {
    fn trait_value(&self) -> impl Future<Output = u32> + '_;
    fn trait_values(&self) -> impl Stream<Item = u32> + 'static;
}
#[uniffi::export(local)]
impl LocalBusiness for LocalCounter {
    fn trait_value(&self) -> impl Future<Output = u32> + '_ {
        async move { self.state.get() }
    }
    fn trait_values(&self) -> impl Stream<Item = u32> + 'static {
        LocalValues {
            state: self.state.clone(),
            remaining: 1,
            owner: std::thread::current().id(),
        }
    }
}
#[uniffi::export(local)]
impl LocalCounter {
    pub fn values(&self) -> impl Stream<Item = u32> + 'static {
        LocalValues {
            state: self.state.clone(),
            remaining: 2,
            owner: std::thread::current().id(),
        }
    }
    pub fn value(&self) -> impl Future<Output = u32> + '_ {
        async move { self.state.get() }
    }
}
struct LocalValues {
    state: Rc<Cell<u32>>,
    remaining: u32,
    owner: std::thread::ThreadId,
}
impl Stream for LocalValues {
    type Item = u32;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u32>> {
        assert_eq!(self.owner, std::thread::current().id());
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        self.remaining -= 1;
        self.state.set(self.state.get() + 1);
        Poll::Ready(Some(self.state.get()))
    }
}
impl Drop for LocalValues {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
    }
}

uniffi::setup_scaffolding!();

#[cfg(all(feature = "macro-scaffolding", not(target_arch = "wasm32")))]
mod foreign_lifetime {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[uniffi::export(foreign)]
    pub trait OwnedProvider: Send + Sync {
        fn owned(&self) -> BoxFuture<'static, u32>;
        fn tied<'a>(&'a self) -> BoxFuture<'a, u32>;
    }

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static OBSERVED: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn clone(handle: u64) -> u64 {
        LIVE.fetch_add(1, Ordering::SeqCst);
        handle
    }
    extern "C" fn free(_: u64) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
    extern "C" fn value(
        _: u64,
        callback: uniffi::ForeignFutureCallback<u32>,
        data: u64,
        _: &mut uniffi::ForeignFutureDroppedCallbackStruct,
    ) {
        OBSERVED.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
        callback(
            data,
            uniffi::ForeignFutureResult::from_raw_parts(42, uniffi::RustCallStatus::default()),
        );
    }

    #[test]
    fn returned_future_owns_foreign_receiver_until_completed_or_dropped() {
        let vtable = Box::leak(Box::new(UniFfiTraitVtableOwnedProvider {
            uniffi_clone: clone,
            uniffi_free: free,
            owned: value,
            tied: value,
        }));
        UNIFFI_TRAIT_CELL_OWNEDPROVIDER.set(std::ptr::NonNull::from(vtable));
        LIVE.store(1, Ordering::SeqCst);
        let handler = UniFFICallbackHandlerOwnedProvider::new(7);
        let future = handler.owned();
        assert_eq!(LIVE.load(Ordering::SeqCst), 2);
        drop(handler);
        assert_eq!(LIVE.load(Ordering::SeqCst), 1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(future), 42);
        assert_eq!(OBSERVED.load(Ordering::SeqCst), 1);
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);

        LIVE.store(1, Ordering::SeqCst);
        let handler = UniFFICallbackHandlerOwnedProvider::new(7);
        let future = handler.owned();
        drop(handler);
        drop(future);
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn native_declarations_and_dyn_adapter_keep_rust_behavior() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            assert_eq!(future_value(1).await, 2);
            assert_eq!(boxed_value(1).await, Ok(3));
            let provider: Arc<dyn Provider> = Arc::new(RustProvider);
            assert_eq!(provider.value(1).await, 5);
            assert_eq!(provider.values().collect::<Vec<_>>().await, vec![8, 9]);
            assert_eq!(
                provider
                    .values_later()
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await,
                vec![Ok(10)]
            );
            assert_eq!(
                provider.boxed_values().await.collect::<Vec<_>>().await,
                vec![Ok(11)]
            );
            let owner = LocalExecutor::new(2).unwrap();
            let proxy: Arc<LocalObject<LocalCounter>> = Arc::new(
                owner
                    .create(|| async {
                        LocalCounter {
                            state: Rc::new(Cell::new(20)),
                        }
                    })
                    .await
                    .unwrap(),
            );
            let stream = LocalCounter::__uniffi_local_values(proxy.clone())
                .await
                .unwrap();
            fn assert_send<T: Send>(_: &T) {}
            assert_send(&stream);
            assert_eq!(stream.collect::<Vec<_>>().await, vec![Ok(21), Ok(22)]);
            assert_eq!(
                LocalCounter::__uniffi_local_value(proxy.clone())
                    .await
                    .unwrap(),
                22
            );
            assert_eq!(
                LocalCounter::__uniffi_local_trait_value(proxy.clone())
                    .await
                    .unwrap(),
                22
            );
            let values = LocalCounter::__uniffi_local_trait_values(proxy.clone())
                .await
                .unwrap();
            assert_eq!(values.collect::<Vec<_>>().await, vec![Ok(23)]);
            LocalCounter::__uniffi_local_close(proxy).await.unwrap();
            owner.close().await.unwrap();
        });
}

#[cfg(all(feature = "macro-scaffolding", not(target_arch = "wasm32")))]
fn complete<T: uniffi::FfiDefault>(future: uniffi::Handle) -> (T, uniffi::RustCallStatus) {
    extern "C" fn wake(data: u64, state: uniffi::RustFuturePoll) {
        // Each poll transfers one sender and receives exactly one callback.
        let sender =
            unsafe { Box::from_raw(data as *mut std::sync::mpsc::Sender<uniffi::RustFuturePoll>) };
        sender.send(state).unwrap();
    }
    loop {
        let (send, receive) = std::sync::mpsc::channel::<uniffi::RustFuturePoll>();
        let data = Box::into_raw(Box::new(send)) as u64;
        unsafe {
            uniffi::rust_future_poll::<T>(future.clone(), wake, data);
        }
        if receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            == uniffi::RustFuturePoll::Ready
        {
            break;
        }
    }
    let mut status = uniffi::RustCallStatus::default();
    let value = unsafe { uniffi::rust_future_complete::<T>(future.clone(), &mut status) };
    unsafe {
        uniffi::rust_future_free::<T>(future);
    }
    (value, status)
}

#[test]
#[cfg(all(feature = "macro-scaffolding", not(target_arch = "wasm32")))]
fn generated_ffi_factories_preserve_future_handles_and_stream_steps() {
    let (value, status) = complete::<u32>(uniffi_uniffi_fn_func_future_value(4));
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    assert_eq!(value, 5);
    let mut status = uniffi::RustCallStatus::default();
    let stream = uniffi_uniffi_fn_func_optional_values(&mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    for expected in [
        uniffi::UniFfiStreamStep::Item(None),
        uniffi::UniFfiStreamStep::Item(Some(7)),
        uniffi::UniFfiStreamStep::Done,
    ] {
        let (bytes, status) = complete::<uniffi::RustBuffer>(
            uniffi_uniffi_fn_func_optional_values_stream_next(stream.clone()),
        );
        assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
        let step =
            <uniffi::UniFfiStreamStep<Option<u32>, std::convert::Infallible> as uniffi::Lift<
                UniFfiTag,
            >>::try_lift(bytes)
            .unwrap();
        assert_eq!(step, expected);
    }
    let (stream, status) = complete::<uniffi::Handle>(uniffi_uniffi_fn_func_make_values(0));
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    for expected in [
        uniffi::UniFfiStreamStep::Item(3),
        uniffi::UniFfiStreamStep::Error(Failure::Item),
        uniffi::UniFfiStreamStep::Done,
    ] {
        let (bytes, status) = complete::<uniffi::RustBuffer>(
            uniffi_uniffi_fn_func_make_values_stream_next(stream.clone()),
        );
        assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
        let step =
            <uniffi::UniFfiStreamStep<u32, Failure> as uniffi::Lift<UniFfiTag>>::try_lift(bytes)
                .unwrap();
        assert_eq!(step, expected);
    }
    let (_, status) = complete::<uniffi::Handle>(uniffi_uniffi_fn_func_make_values(1));
    assert_eq!(status.code, uniffi::RustCallStatusCode::Error);
    let error = <Failure as uniffi::Lift<UniFfiTag>>::try_lift(std::mem::ManuallyDrop::into_inner(
        status.error_buf,
    ))
    .unwrap();
    assert_eq!(error, Failure::Factory);
}
