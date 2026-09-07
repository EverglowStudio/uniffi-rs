#![cfg(all(
    feature = "async-runtime",
    feature = "macro-scaffolding",
    not(target_arch = "wasm32")
))]

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Once,
    },
    task::{Context, Poll, Waker},
};
use uniffi::{
    async_runtime::{CallContext, CancellationToken, Cancelled},
    ForeignFutureCallback, ForeignFutureDroppedCallbackStruct, ForeignFutureResult,
    ForeignInputStreamCloseFuture, ForeignInputStreamNextFuture, ForeignInputStreamOps, Handle,
    LiftReturn, LowerReturn, RustCallStatus, UniFfiInputStream, UniFfiStream,
};

uniffi::setup_scaffolding!();

#[uniffi::export(with_foreign)]
pub trait StreamHost: Send + Sync {
    fn echo(&self, handle: UniFfiInputStream<u32, Infallible>) -> UniFfiStream<u32, Infallible>;
    async fn echo_async(
        &self,
        handle: UniFfiInputStream<u32, Infallible>,
    ) -> UniFfiStream<u32, Infallible>;
    async fn create(&self) -> UniFfiStream<u32, Infallible>;
}

#[derive(Default)]
struct HostState {
    factory: Option<(ForeignFutureCallback<Handle>, u64)>,
    close: Option<(ForeignFutureCallback<()>, u64)>,
    cancelled: usize,
    factory_cancelled: usize,
}
static HOST: Mutex<HostState> = Mutex::new(HostState {
    factory: None,
    close: None,
    cancelled: 0,
    factory_cancelled: 0,
});
static SERIAL: Mutex<()> = Mutex::new(());

extern "C" fn clone_handle(handle: u64) -> u64 {
    handle
}
extern "C" fn free_handle(_: u64) {}
extern "C" fn cancel_factory(_: u64) {
    HOST.lock().unwrap().factory_cancelled += 1;
}
extern "C" fn create(
    _: u64,
    callback: ForeignFutureCallback<Handle>,
    data: u64,
    dropped: &mut ForeignFutureDroppedCallbackStruct,
) {
    HOST.lock().unwrap().factory = Some((callback, data));
    *dropped = ForeignFutureDroppedCallbackStruct {
        callback_data: 0,
        callback: cancel_factory,
    };
}
extern "C" fn echo(_: u64, _: Handle, value: &mut Handle, _: &mut RustCallStatus) {
    *value = Handle::from_raw_unchecked(99);
}
extern "C" fn echo_async(
    _: u64,
    _: Handle,
    callback: ForeignFutureCallback<Handle>,
    data: u64,
    _: &mut ForeignFutureDroppedCallbackStruct,
) {
    callback(
        data,
        ForeignFutureResult::from_raw_parts(
            Handle::from_raw_unchecked(99),
            RustCallStatus::default(),
        ),
    );
}
type NextReturn = <Result<Option<u32>, Infallible> as LiftReturn<UniFfiTag>>::ReturnType;
extern "C" fn next(
    _: Handle,
    callback: ForeignFutureCallback<NextReturn>,
    data: u64,
    _: &mut ForeignFutureDroppedCallbackStruct,
) {
    let value =
        <Result<Option<u32>, Infallible> as LowerReturn<UniFfiTag>>::lower_return(Ok(Some(7)))
            .unwrap_or_else(|_| panic!("valid scalar stream item must lower"));
    callback(
        data,
        ForeignFutureResult::from_raw_parts(value, RustCallStatus::default()),
    );
}
extern "C" fn cancel(_: Handle) {
    HOST.lock().unwrap().cancelled += 1;
}
extern "C" fn close(
    _: Handle,
    callback: ForeignFutureCallback<()>,
    data: u64,
    _: &mut ForeignFutureDroppedCallbackStruct,
) {
    assert!(HOST
        .lock()
        .unwrap()
        .close
        .replace((callback, data))
        .is_none());
}

fn setup() -> UniFFICallbackHandlerStreamHost {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let vtable = Box::leak(Box::new(UniFfiTraitVtableStreamHost {
            uniffi_clone: clone_handle,
            uniffi_free: free_handle,
            echo,
            echo_stream_next: next,
            echo_stream_cancel: cancel,
            echo_stream_close: close,
            echo_async,
            echo_async_stream_next: next,
            echo_async_stream_cancel: cancel,
            echo_async_stream_close: close,
            create,
            create_stream_next: next,
            create_stream_cancel: cancel,
            create_stream_close: close,
        }));
        UNIFFI_TRAIT_CELL_STREAMHOST.set(std::ptr::NonNull::from(vtable));
    });
    *HOST.lock().unwrap() = HostState::default();
    UniFFICallbackHandlerStreamHost::new(1)
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
fn complete_factory() {
    let (callback, data) = HOST.lock().unwrap().factory.take().unwrap();
    callback(
        data,
        ForeignFutureResult::from_raw_parts(
            Handle::from_raw_unchecked(99),
            RustCallStatus::default(),
        ),
    );
}
fn complete_close() {
    let (callback, data) = HOST.lock().unwrap().close.take().unwrap();
    callback(
        data,
        ForeignFutureResult::from_raw_parts((), RustCallStatus::default()),
    );
}

#[test]
fn cancelled_factory_disposes_unreceived_success_and_waits_for_stream_close() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for completion_before_cancel_poll in [false, true] {
        let host = setup();
        let context = CallContext::new(CancellationToken::new());
        let mut call = Box::pin(context.run(async move { host.create().await }));
        assert!(poll(call.as_mut()).is_pending());
        if completion_before_cancel_poll {
            complete_factory();
        }
        context.cancel();
        if !completion_before_cancel_poll {
            assert!(poll(call.as_mut()).is_pending());
            complete_factory();
        }
        assert!(
            poll(call.as_mut()).is_pending(),
            "an unreceived successful stream still needs its close acknowledgement"
        );
        assert_eq!(HOST.lock().unwrap().cancelled, 1);
        assert!(HOST.lock().unwrap().close.is_some());
        complete_close();
        assert!(matches!(poll(call.as_mut()), Poll::Ready(Err(Cancelled))));
        assert_eq!(HOST.lock().unwrap().factory_cancelled, 1);
    }
}

#[derive(Clone)]
struct InputOps {
    cancelled: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    gate: CancellationToken,
}
impl ForeignInputStreamOps<u32, Infallible> for InputOps {
    fn next(&self, _: Handle) -> ForeignInputStreamNextFuture<u32, Infallible> {
        Box::pin(async { Ok(Some(7)) })
    }
    fn cancel(&self, _: Handle) {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
    }
    fn close(&self, _: Handle) -> ForeignInputStreamCloseFuture {
        let this = self.clone();
        Box::pin(async move {
            this.gate.cancelled().await;
            this.closed.fetch_add(1, Ordering::SeqCst);
        })
    }
}

#[test]
fn reverse_sync_and_async_stream_factories_retain_inputs_until_output_closes() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for is_async in [false, true] {
        let host = setup();
        let ops = InputOps {
            cancelled: Arc::new(AtomicUsize::new(0)),
            closed: Arc::new(AtomicUsize::new(0)),
            gate: CancellationToken::new(),
        };
        let source = UniFfiInputStream::new(Handle::from_raw_unchecked(11), ops.clone());
        let context = CallContext::new(CancellationToken::new());
        let mut factory = Box::pin(context.run(async move {
            if is_async {
                host.echo_async(source).await
            } else {
                host.echo(source)
            }
        }));
        let Poll::Ready(Ok(mut output)) = poll(factory.as_mut()) else {
            panic!("a returned stream must own the input without closing it during its factory");
        };
        assert_eq!(ops.cancelled.load(Ordering::SeqCst), 0);
        assert_eq!(
            output
                .as_mut()
                .poll_next(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Some(Ok(7)))
        );
        assert_eq!(ops.cancelled.load(Ordering::SeqCst), 0);
        let close_context = CallContext::new(CancellationToken::new());
        let mut closing = Box::pin(close_context.run(async move {
            drop(output);
        }));
        assert!(poll(closing.as_mut()).is_pending());
        assert_eq!(HOST.lock().unwrap().cancelled, 1);
        assert_eq!(
            ops.cancelled.load(Ordering::SeqCst),
            0,
            "the input remains alive until the returned producer acknowledges close"
        );
        complete_close();
        assert!(poll(closing.as_mut()).is_pending());
        assert_eq!(ops.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(ops.closed.load(Ordering::SeqCst), 0);
        ops.gate.cancel();
        assert_eq!(poll(closing.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(ops.closed.load(Ordering::SeqCst), 1);
    }
}
