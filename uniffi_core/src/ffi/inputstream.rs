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

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub trait ForeignInputStreamOps<T, E>: Send + Sync + 'static {
    fn next(&self, handle: Handle) -> ForeignInputStreamNextFuture<T, E>;
    fn cancel(&self, handle: Handle);
}

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub trait ForeignInputStreamOps<T, E>: 'static {
    fn next(&self, handle: Handle) -> ForeignInputStreamNextFuture<T, E>;
    fn cancel(&self, handle: Handle);
}

pub type ForeignInputStreamNextCallback<FfiReturn> = extern "C" fn(
    handle: Handle,
    callback: ForeignFutureCallback<FfiReturn>,
    callback_data: u64,
    dropped_callback: &mut ForeignFutureDroppedCallbackStruct,
);

pub type ForeignInputStreamCancelCallback = extern "C" fn(handle: Handle);

struct ForeignInputStreamInner<T: 'static, E: 'static> {
    handle: Handle,
    ops: Arc<dyn ForeignInputStreamOps<T, E>>,
    cancelled: AtomicBool,
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
    done: bool,
}

impl<T: 'static, E: 'static> ForeignInputStream<T, E> {
    pub fn new(handle: Handle, ops: impl ForeignInputStreamOps<T, E>) -> Self {
        Self::from_handle_and_ops(handle, Arc::new(ops))
    }

    pub fn from_handle_and_ops(handle: Handle, ops: Arc<dyn ForeignInputStreamOps<T, E>>) -> Self {
        Self {
            inner: Arc::new(ForeignInputStreamInner {
                handle,
                ops,
                cancelled: AtomicBool::new(false),
            }),
            pending_next: None,
            done: false,
        }
    }

    pub fn from_foreign_callbacks<UT>(
        handle: Handle,
        next: ForeignInputStreamNextCallback<<Result<Option<T>, E> as LiftReturn<UT>>::ReturnType>,
        cancel: ForeignInputStreamCancelCallback,
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
                _phantom: PhantomData,
            },
        )
    }

    pub fn cancel(&self) {
        self.inner.cancel();
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
            self.pending_next = None;
            self.done = true;
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
                self.pending_next = None;
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Err(error)) => {
                self.pending_next = None;
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}

impl<T: 'static, E: 'static> Drop for ForeignInputStream<T, E> {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

struct ForeignInputStreamCallbackOps<T, E, UT>
where
    Result<Option<T>, E>: LiftReturn<UT>,
{
    next: ForeignInputStreamNextCallback<<Result<Option<T>, E> as LiftReturn<UT>>::ReturnType>,
    cancel: ForeignInputStreamCancelCallback,
    _phantom: PhantomData<fn() -> (T, E, UT)>,
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

    #[derive(Clone)]
    struct FakeOps {
        values: Arc<Mutex<VecDeque<Result<Option<u32>, String>>>>,
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

    #[test]
    fn input_stream_ffi_callback_ops_lift_item() {
        let mut stream = UniFfiInputStream::<u32, String>::from_foreign_callbacks::<crate::UniFfiTag>(
            Handle::from_raw_unchecked(1),
            ffi_success_next,
            ffi_cancel,
        );

        assert_eq!(poll_input_stream(&mut stream), Poll::Ready(Some(Ok(99))));
    }

    #[test]
    fn input_stream_ffi_callback_ops_lift_typed_error() {
        let mut stream = UniFfiInputStream::<u32, String>::from_foreign_callbacks::<crate::UniFfiTag>(
            Handle::from_raw_unchecked(1),
            ffi_error_next,
            ffi_cancel,
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
