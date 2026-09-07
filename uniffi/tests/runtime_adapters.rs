#![cfg(all(
    feature = "async-runtime",
    feature = "tokio",
    not(target_arch = "wasm32")
))]

use std::{
    future::{pending, Future},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};
use uniffi::async_runtime::{current_context, CallContext, CancellationToken};

uniffi::setup_scaffolding!();

#[uniffi::export]
pub async fn adapter_plain() -> bool {
    in_runtime()
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn adapter_explicit() -> bool {
    in_runtime()
}

#[derive(uniffi::Object)]
pub struct AdapterObject;

#[uniffi::export(async_runtime = "tokio")]
impl AdapterObject {
    #[uniffi::constructor]
    pub async fn new() -> Arc<Self> {
        Arc::new(Self)
    }
    pub async fn value(&self) -> bool {
        in_runtime()
    }
}

#[uniffi::export(async_runtime = "tokio")]
pub trait AdapterHost: Send + Sync {
    async fn value(&self) -> bool;
}

#[derive(uniffi::Object)]
#[uniffi(local)]
pub struct AdapterLocal;

fn in_runtime() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn drive<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "runtime adapter did not wake cleanup");
                thread::park_timeout(remaining);
            }
        }
    }
}

struct Cleanup(Arc<AtomicBool>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        assert!(
            in_runtime(),
            "business destruction must remain in the selected runtime"
        );
        let done = self.0.clone();
        current_context()
            .unwrap()
            .defer_cleanup(async move {
                assert!(in_runtime(), "cleanup starts inside the selected runtime");
                tokio::time::sleep(Duration::from_millis(1)).await;
                assert!(in_runtime(), "resumed cleanup keeps the selected runtime");
                done.store(true, Ordering::SeqCst);
            })
            .unwrap();
    }
}

#[test]
fn callable_adapters_preserve_explicit_and_default_selection() {
    assert!(!in_runtime());
    assert_eq!(
        drive(uniffi_uniffi_fn_func_adapter_plain_runtime(adapter_plain())),
        cfg!(feature = "default-async-runtime-tokio")
    );
    assert!(drive(uniffi_uniffi_fn_func_adapter_explicit_runtime(
        adapter_explicit()
    )));
    assert!(drive(
        uniffi_uniffi_fn_constructor_adapterobject_new_runtime(async { in_runtime() })
    ));
    assert!(drive(uniffi_uniffi_fn_method_adapterobject_value_runtime(
        async { in_runtime() }
    )));
    assert!(drive(uniffi_uniffi_fn_method_adapterhost_value_runtime(
        async { in_runtime() }
    )));
    assert_eq!(
        drive(uniffi_uniffi_fn_method_adapterlocal_close_runtime(async {
            in_runtime()
        })),
        cfg!(feature = "default-async-runtime-tokio")
    );
    assert!(
        !in_runtime(),
        "the adapter must restore the caller's context"
    );
}

#[test]
fn entire_run_keeps_runtime_through_success_cleanup() {
    let context = CallContext::new(CancellationToken::new());
    let done = Arc::new(AtomicBool::new(false));
    let cleanup = Cleanup(done.clone());
    let run = context.run(async move {
        drop(cleanup);
        7
    });
    assert_eq!(
        drive(uniffi_uniffi_fn_func_adapter_explicit_runtime(run)).unwrap(),
        7
    );
    assert!(done.load(Ordering::SeqCst));
    assert!(!in_runtime());
}

#[test]
fn entire_run_keeps_runtime_through_pre_poll_cancel_cleanup() {
    let context = CallContext::new(CancellationToken::new());
    let done = Arc::new(AtomicBool::new(false));
    let cleanup = Cleanup(done.clone());
    let run = context.run(async move {
        let _cleanup = cleanup;
        pending::<()>().await;
    });
    context.cancel();
    assert!(drive(uniffi_uniffi_fn_func_adapter_explicit_runtime(run)).is_err());
    assert!(done.load(Ordering::SeqCst));
    assert!(!in_runtime());
}

#[test]
fn stream_adapter_matches_native_stream_driver_scope() {
    let context = CallContext::new(CancellationToken::new());
    let done = Arc::new(AtomicBool::new(false));
    let cleanup = Cleanup(done.clone());
    let run = context.run(async move {
        drop(cleanup);
    });
    drive(uniffi::rust_stream_runtime(run)).unwrap();
    assert!(done.load(Ordering::SeqCst));
    assert!(!in_runtime());
}

#[derive(Default)]
struct CancelProbe {
    dropped: std::sync::atomic::AtomicUsize,
    dropped_in_runtime: AtomicBool,
    cleaned: std::sync::atomic::AtomicUsize,
    cleanup_in_runtime: AtomicBool,
}
struct CancelStream {
    probe: Arc<CancelProbe>,
    gate: CancellationToken,
}
impl uniffi::deps::futures_core::Stream for CancelStream {
    type Item = Result<u32, std::convert::Infallible>;
    fn poll_next(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}
impl Drop for CancelStream {
    fn drop(&mut self) {
        self.probe
            .dropped_in_runtime
            .store(in_runtime(), Ordering::SeqCst);
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
        let probe = self.probe.clone();
        let gate = self.gate.clone();
        current_context()
            .unwrap()
            .defer_cleanup(async move {
                let entered = in_runtime();
                gate.cancelled().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
                probe
                    .cleanup_in_runtime
                    .store(entered && in_runtime(), Ordering::SeqCst);
                probe.cleaned.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
    }
}
fn sync_stream_cancel_keeps_runtime(poll_first: bool) {
    use uniffi::{
        rust_stream_cancel, rust_stream_close_async, rust_stream_new, rust_stream_next_async,
        rust_stream_runtime, RustStreamRegistry,
    };
    static STREAMS: RustStreamRegistry<u32, std::convert::Infallible> =
        RustStreamRegistry::new(Default::default);
    assert!(!in_runtime());
    let probe = Arc::new(CancelProbe::default());
    let gate = CancellationToken::new();
    let handle = rust_stream_new(
        &STREAMS,
        Box::pin(CancelStream {
            probe: probe.clone(),
            gate: gate.clone(),
        }),
    );
    let mut next = poll_first.then(|| {
        Box::pin(rust_stream_runtime(rust_stream_next_async(
            &STREAMS,
            handle.clone(),
        )))
    });
    if let Some(next) = next.as_mut() {
        assert!(next
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
    }
    rust_stream_cancel(&STREAMS, handle.clone());
    assert!(
        !in_runtime(),
        "cancel restores its caller's runtime context"
    );
    assert_eq!(probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(probe.cleaned.load(Ordering::SeqCst), 0);
    let mut close = Box::pin(rust_stream_runtime(rust_stream_close_async(
        &STREAMS, handle,
    )));
    assert!(close
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    if let Some(next) = next.as_mut() {
        assert!(next
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
    }
    gate.cancel();
    drive(close).unwrap();
    if let Some(next) = next {
        assert!(drive(next).is_err_and(|error| error.error.is::<uniffi::StreamCancelled>()));
    }
    assert_eq!(probe.cleaned.load(Ordering::SeqCst), 1);
    assert!(probe.cleanup_in_runtime.load(Ordering::SeqCst));
    assert!(
        probe.dropped_in_runtime.load(Ordering::SeqCst),
        "synchronous cancellation dropped the stream outside its selected runtime"
    );
}
#[test]
fn sync_stream_cancel_before_next_keeps_runtime_through_cleanup() {
    sync_stream_cancel_keeps_runtime(false);
}
#[test]
fn sync_stream_cancel_after_pending_next_keeps_runtime_through_cleanup() {
    sync_stream_cancel_keeps_runtime(true);
}
