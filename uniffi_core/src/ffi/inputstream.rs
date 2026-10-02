/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    foreign_async_call, ForeignFutureCallback, ForeignFutureDroppedCallbackStruct, Handle, Lift,
    LiftReturn, MetadataBuffer, TypeId,
};
use anyhow::{bail, Result};
use futures_core::Stream;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

/// Foreign-owned input stream exposed to exported Rust functions.
///
/// Backends create this adapter from a foreign stream handle plus `next` and
/// `cancel` operations. `poll_next` keeps a single in-flight `next` operation;
/// callers cannot poll the same stream concurrently without also violating the
/// `Stream::poll_next(&mut self, ...)` contract.
pub type UniFfiInputStream<T, E> = ForeignInputStream<T, E>;

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub type ForeignInputStreamNextFuture<T, E> =
    Pin<Box<dyn Future<Output = Result<Option<T>, E>> + Send + 'static>>;

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub type ForeignInputStreamNextFuture<T, E> =
    Pin<Box<dyn Future<Output = Result<Option<T>, E>> + 'static>>;

/// Completion must acknowledge producer, registration and callback release.
#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub type ForeignInputStreamCloseFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub type ForeignInputStreamCloseFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[cfg(feature = "async-runtime")]
use crate::async_runtime::{current_context, BoxFuture, CleanupFailed};
#[cfg(feature = "async-runtime")]
use futures_util::{future::Shared, FutureExt};
#[cfg(feature = "async-runtime")]
type CloseDone = Shared<BoxFuture<'static, Result<(), CleanupFailed>>>;

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub trait ForeignInputStreamOps<T, E>: Send + Sync + 'static {
    fn next(&self, handle: Handle) -> ForeignInputStreamNextFuture<T, E>;
    fn cancel(&self, handle: Handle);
    fn close(&self, handle: Handle) -> ForeignInputStreamCloseFuture;
}

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub trait ForeignInputStreamOps<T, E>: 'static {
    fn next(&self, handle: Handle) -> ForeignInputStreamNextFuture<T, E>;
    fn cancel(&self, handle: Handle);
    fn close(&self, handle: Handle) -> ForeignInputStreamCloseFuture;
}

pub type ForeignInputStreamNextCallback<FfiReturn> = extern "C" fn(
    handle: Handle,
    callback: ForeignFutureCallback<FfiReturn>,
    callback_data: u64,
    dropped_callback: &mut ForeignFutureDroppedCallbackStruct,
);

pub type ForeignInputStreamCancelCallback = extern "C" fn(handle: Handle);
pub type ForeignInputStreamCloseCallback = extern "C" fn(
    handle: Handle,
    callback: ForeignFutureCallback<()>,
    callback_data: u64,
    dropped_callback: &mut ForeignFutureDroppedCallbackStruct,
);

struct ForeignInputStreamInner<T: 'static, E: 'static> {
    handle: Handle,
    ops: Arc<dyn ForeignInputStreamOps<T, E>>,
    cancelled: AtomicBool,
    #[cfg(feature = "async-runtime")]
    closed: CloseDone,
}

impl<T: 'static, E: 'static> ForeignInputStreamInner<T, E> {
    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.ops.cancel(self.handle.clone());
        }
    }
}

pub struct ForeignInputStream<T: 'static, E: 'static> {
    inner: Arc<ForeignInputStreamInner<T, E>>,
    pending_next: Option<ForeignInputStreamNextFuture<T, E>>,
    #[cfg(not(feature = "async-runtime"))]
    retained: Vec<Box<dyn crate::UniffiCompatibleStreamValue>>,
    done: bool,
}

impl<T: 'static, E: 'static> ForeignInputStream<T, E> {
    pub fn new(handle: Handle, ops: impl ForeignInputStreamOps<T, E>) -> Self {
        Self::from_handle_and_ops(handle, Arc::new(ops))
    }

    pub fn from_handle_and_ops(handle: Handle, ops: Arc<dyn ForeignInputStreamOps<T, E>>) -> Self {
        #[cfg(feature = "async-runtime")]
        let closed = {
            let ops = ops.clone();
            let handle = handle.clone();
            let future: BoxFuture<'static, Result<(), CleanupFailed>> = Box::pin(async move {
                std::panic::AssertUnwindSafe(async move { ops.close(handle).await })
                    .catch_unwind()
                    .await
                    .map_err(|_| CleanupFailed)
            });
            future.shared()
        };
        Self {
            inner: Arc::new(ForeignInputStreamInner {
                handle,
                ops,
                cancelled: AtomicBool::new(false),
                #[cfg(feature = "async-runtime")]
                closed,
            }),
            pending_next: None,
            #[cfg(not(feature = "async-runtime"))]
            retained: Vec::new(),
            done: false,
        }
    }

    pub fn from_foreign_callbacks<UT>(
        handle: Handle,
        next: ForeignInputStreamNextCallback<<Result<Option<T>, E> as LiftReturn<UT>>::ReturnType>,
        cancel: ForeignInputStreamCancelCallback,
        close: ForeignInputStreamCloseCallback,
    ) -> Self
    where
        Result<Option<T>, E>: LiftReturn<UT> + 'static,
        <Result<Option<T>, E> as LiftReturn<UT>>::ReturnType: Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
        UT: Send + Sync + 'static,
    {
        Self::new(
            handle,
            ForeignInputStreamCallbackOps::<T, E, UT> {
                next,
                cancel,
                close,
                _phantom: PhantomData,
            },
        )
    }

    /// Keep arguments moved into a returned foreign producer until that
    /// producer acknowledges close, then release them in the same call scope.
    #[doc(hidden)]
    pub fn retain_until_close<R: crate::UniffiCompatibleStreamValue>(
        mut self,
        resources: R,
    ) -> Self {
        #[cfg(feature = "async-runtime")]
        {
            let closed = self.inner.closed.clone();
            let future: BoxFuture<'static, Result<(), CleanupFailed>> = Box::pin(async move {
                let result = closed.await;
                drop(resources);
                result
            });
            // Only this stream owns inner. Confirmation waiters share closed,
            // and its future owns ops; neither clones the inner allocation.
            Arc::get_mut(&mut self.inner)
                .expect("foreign stream has one inner owner")
                .closed = future.shared();
        }
        #[cfg(not(feature = "async-runtime"))]
        self.retained.push(Box::new(resources));
        self
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Stop next and await the host's actual release acknowledgement. A dropped
    /// waiter transfers its shared confirmation to the surrounding CallContext.
    #[cfg(feature = "async-runtime")]
    pub async fn close(&mut self) -> Result<(), CleanupFailed> {
        self.done = true;
        drop(self.pending_next.take());
        self.inner.cancel();
        let mut guard = InputCloseOnDrop(Some(self.inner.closed.clone()));
        let result = self.inner.closed.clone().await;
        guard.0 = None;
        result
    }

    fn finish(&mut self) {
        self.done = true;
        drop(self.pending_next.take());
        self.inner.cancel();
        #[cfg(not(feature = "async-runtime"))]
        self.retained.clear();
        #[cfg(feature = "async-runtime")]
        drop(InputCloseOnDrop(Some(self.inner.closed.clone())));
    }

    pub fn handle(&self) -> Handle {
        self.inner.handle.clone()
    }
}

impl<T: 'static, E: 'static> Unpin for ForeignInputStream<T, E> {}

impl<T: 'static, E: 'static> Stream for ForeignInputStream<T, E> {
    type Item = Result<T, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.inner.cancelled.load(Ordering::Acquire) {
            self.finish();
            return Poll::Ready(None);
        }

        if self.pending_next.is_none() {
            self.pending_next = Some(self.inner.ops.next(self.inner.handle.clone()));
        }

        let next = self.pending_next.as_mut().expect("pending next is set");
        match next.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Some(value))) => {
                self.pending_next = None;
                Poll::Ready(Some(Ok(value)))
            }
            Poll::Ready(Ok(None)) => {
                self.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Err(error)) => {
                self.finish();
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}

impl<T: 'static, E: 'static> Drop for ForeignInputStream<T, E> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(feature = "async-runtime")]
struct InputCloseOnDrop(Option<CloseDone>);
#[cfg(feature = "async-runtime")]
impl Drop for InputCloseOnDrop {
    fn drop(&mut self) {
        let Some(done) = self.0.take() else {
            return;
        };
        let cleanup = async move {
            done.await.expect("foreign stream cleanup failed");
        };
        if let Some(context) = current_context() {
            context
                .defer_cleanup(cleanup)
                .expect("current call accepts input stream cleanup");
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                drop(runtime.spawn(cleanup));
            }
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(cleanup);
        }
    }
}

// Keep the covariant, non-owning function-pointer marker and its Send/Sync semantics.
type InputStreamCallbackMarker<T, E, UT> = PhantomData<fn() -> (T, E, UT)>;

struct ForeignInputStreamCallbackOps<T, E, UT>
where
    Result<Option<T>, E>: LiftReturn<UT>,
{
    next: ForeignInputStreamNextCallback<<Result<Option<T>, E> as LiftReturn<UT>>::ReturnType>,
    cancel: ForeignInputStreamCancelCallback,
    close: ForeignInputStreamCloseCallback,
    _phantom: InputStreamCallbackMarker<T, E, UT>,
}

impl<T, E, UT> ForeignInputStreamOps<T, E> for ForeignInputStreamCallbackOps<T, E, UT>
where
    Result<Option<T>, E>: LiftReturn<UT> + 'static,
    <Result<Option<T>, E> as LiftReturn<UT>>::ReturnType: Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    UT: Send + Sync + 'static,
{
    fn next(&self, handle: Handle) -> ForeignInputStreamNextFuture<T, E> {
        let next = self.next;
        Box::pin(foreign_async_call::<_, Result<Option<T>, E>, UT>(
            move |callback, callback_data, dropped_callback| {
                next(handle, callback, callback_data, dropped_callback);
            },
        ))
    }

    fn cancel(&self, handle: Handle) {
        (self.cancel)(handle);
    }
    fn close(&self, handle: Handle) -> ForeignInputStreamCloseFuture {
        let close = self.close;
        Box::pin(foreign_async_call::<_, (), UT>(
            move |callback, data, dropped| {
                close(handle, callback, data, dropped);
            },
        ))
    }
}

unsafe impl<UT, T: 'static, E: 'static> Lift<UT> for ForeignInputStream<T, E> {
    type FfiType = Handle;

    fn try_lift(_v: Self::FfiType) -> Result<Self> {
        bail!("input stream arguments are not wired into this UniFFI backend yet")
    }

    fn try_read(_buf: &mut &[u8]) -> Result<Self> {
        bail!("input stream values are only supported as direct function arguments")
    }
}

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
impl<UT, T: 'static, E: 'static> TypeId<UT> for ForeignInputStream<T, E>
where
    T: TypeId<UT>,
    E: TypeId<UT>,
{
    const TYPE_ID_META: MetadataBuffer =
        MetadataBuffer::from_code(crate::metadata::codes::TYPE_INPUT_STREAM)
            .concat(T::TYPE_ID_META)
            .concat(E::TYPE_ID_META)
            .concat_bool(true);
}

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
impl<UT, T: 'static, E: 'static> TypeId<UT> for ForeignInputStream<T, E>
where
    T: TypeId<UT>,
    E: TypeId<UT>,
{
    const TYPE_ID_META: MetadataBuffer =
        MetadataBuffer::from_code(crate::metadata::codes::TYPE_INPUT_STREAM)
            .concat(T::TYPE_ID_META)
            .concat(E::TYPE_ID_META)
            .concat_bool(false);
}

#[cfg(test)]
mod input_stream_tests {
    use super::*;
    use crate::{
        metadata, ForeignFutureResult, Lower, RustBuffer, RustCallStatus, RustCallStatusCode,
    };
    use std::mem::ManuallyDrop;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        task::{Poll, Wake},
    };

    type InputValueQueue = Arc<Mutex<VecDeque<Result<Option<u32>, String>>>>;

    #[derive(Clone)]
    struct FakeOps {
        values: InputValueQueue,
        cancels: Arc<AtomicUsize>,
    }

    impl FakeOps {
        fn new(values: impl IntoIterator<Item = Result<Option<u32>, String>>) -> Self {
            Self {
                values: Arc::new(Mutex::new(values.into_iter().collect())),
                cancels: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn cancel_count(&self) -> usize {
            self.cancels.load(Ordering::Relaxed)
        }
    }

    impl ForeignInputStreamOps<u32, String> for FakeOps {
        fn next(&self, _handle: Handle) -> ForeignInputStreamNextFuture<u32, String> {
            let value = self.values.lock().unwrap().pop_front().unwrap_or(Ok(None));
            Box::pin(async move { value })
        }

        fn cancel(&self, _handle: Handle) {
            self.cancels.fetch_add(1, Ordering::Relaxed);
        }
        fn close(&self, _: Handle) -> ForeignInputStreamCloseFuture {
            Box::pin(async {})
        }
    }

    struct NoopWaker;

    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    fn poll_input_stream(
        stream: &mut UniFfiInputStream<u32, String>,
    ) -> Poll<Option<Result<u32, String>>> {
        let waker = Arc::new(NoopWaker).into();
        let mut cx = Context::from_waker(&waker);
        Pin::new(stream).poll_next(&mut cx)
    }

    fn input_stream(
        values: impl IntoIterator<Item = Result<Option<u32>, String>>,
    ) -> (UniFfiInputStream<u32, String>, FakeOps) {
        let ops = FakeOps::new(values);
        (
            UniFfiInputStream::new(Handle::from_raw_unchecked(1), ops.clone()),
            ops,
        )
    }

    #[test]
    fn input_stream_yields_multiple_items_then_done() {
        let (mut stream, ops) = input_stream([Ok(Some(10)), Ok(Some(20)), Ok(None)]);

        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(Some(Ok(10))));
        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(Some(Ok(20))));
        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(None));
        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(None));

        drop(stream);
        assert_eq!(ops.cancel_count(), 1);
    }

    #[test]
    fn input_stream_returns_done() {
        let (mut stream, _ops) = input_stream([Ok(None)]);

        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(None));
    }

    #[test]
    fn input_stream_returns_typed_error() {
        let (mut stream, _ops) = input_stream([Err("typed failure".to_string())]);

        assert_eq!(
            poll_input_stream(&mut stream),
            Poll::Ready(Some(Err("typed failure".to_string())))
        );
        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(None));
    }

    #[test]
    fn input_stream_cancel_is_idempotent() {
        let (mut stream, ops) = input_stream([Ok(Some(10))]);

        stream.cancel();
        stream.cancel();
        assert_eq!(ops.cancel_count(), 1);
        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(None));

        drop(stream);
        assert_eq!(ops.cancel_count(), 1);
    }

    #[test]
    fn input_stream_drop_triggers_cancel() {
        let (stream, ops) = input_stream([Ok(Some(10))]);

        drop(stream);
        assert_eq!(ops.cancel_count(), 1);
    }

    extern "C" fn ffi_success_next(
        _handle: Handle,
        callback: ForeignFutureCallback<RustBuffer>,
        callback_data: u64,
        _dropped_callback: &mut ForeignFutureDroppedCallbackStruct,
    ) {
        callback(
            callback_data,
            ForeignFutureResult::from_raw_parts(
                <Option<u32> as Lower<crate::UniFfiTag>>::lower(Some(99)),
                RustCallStatus::default(),
            ),
        );
    }

    extern "C" fn ffi_error_next(
        _handle: Handle,
        callback: ForeignFutureCallback<RustBuffer>,
        callback_data: u64,
        _dropped_callback: &mut ForeignFutureDroppedCallbackStruct,
    ) {
        callback(
            callback_data,
            ForeignFutureResult::from_raw_parts(
                RustBuffer::default(),
                RustCallStatus {
                    code: RustCallStatusCode::Error,
                    error_buf: ManuallyDrop::new(
                        <String as Lower<crate::UniFfiTag>>::lower_into_rust_buffer(
                            "lifted error".to_string(),
                        ),
                    ),
                },
            ),
        );
    }

    extern "C" fn ffi_cancel(_handle: Handle) {}
    extern "C" fn ffi_close(
        _: Handle,
        callback: ForeignFutureCallback<()>,
        data: u64,
        _: &mut ForeignFutureDroppedCallbackStruct,
    ) {
        callback(
            data,
            ForeignFutureResult::from_raw_parts((), RustCallStatus::default()),
        );
    }

    #[test]
    fn input_stream_ffi_callback_ops_lift_item() {
        let mut stream = UniFfiInputStream::<u32, String>::from_foreign_callbacks::<crate::UniFfiTag>(
            Handle::from_raw_unchecked(1),
            ffi_success_next,
            ffi_cancel,
            ffi_close,
        );

        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(Some(Ok(99))));
    }

    #[test]
    fn input_stream_ffi_callback_ops_lift_typed_error() {
        let mut stream = UniFfiInputStream::<u32, String>::from_foreign_callbacks::<crate::UniFfiTag>(
            Handle::from_raw_unchecked(1),
            ffi_error_next,
            ffi_cancel,
            ffi_close,
        );

        assert_eq!(
            poll_input_stream(&mut stream),
            Poll::Ready(Some(Err("lifted error".to_string())))
        );
    }

    #[test]
    fn input_stream_type_id_metadata_marks_input_direction() {
        type Input = UniFfiInputStream<u32, String>;

        let meta = <Input as TypeId<crate::UniFfiTag>>::TYPE_ID_META;
        assert_eq!(
            &meta.bytes[..meta.size],
            &[
                metadata::codes::TYPE_INPUT_STREAM,
                metadata::codes::TYPE_U32,
                metadata::codes::TYPE_STRING,
                1,
            ]
        );
    }
}
