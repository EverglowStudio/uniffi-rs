//! Small, runtime-independent workloads for the existing foreign async ABI.
use std::{
    future::{pending, poll_fn},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    task::{Poll, Waker},
};

// One persistent worker; no timer floor or per-operation thread creation.
struct WakeWorker {
    sender: mpsc::Sender<Option<Waker>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}
static WAKE_WORKER: OnceLock<WakeWorker> = OnceLock::new();

#[cfg(feature = "uniffi-bindgen-kotlin-jni")]
static SHUTDOWN_JVM: std::sync::atomic::AtomicPtr<uniffi_bindgen_kotlin_jni_runtime::JavaVM> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

// The generated scaffold's VM accessor is private. This fixture-only JNI
// teardown entry obtains the VM from the calling Java thread without changing
// the generated runtime or attaching/detaching during timed operations.
#[cfg(feature = "uniffi-bindgen-kotlin-jni")]
#[no_mangle]
unsafe extern "system" fn Java_StreamsJniKt_shutdownWakeWorker(
    env: *mut uniffi_bindgen_kotlin_jni_runtime::JNIEnv,
    _class: uniffi_bindgen_kotlin_jni_runtime::jclass,
) {
    let mut jvm = std::ptr::null_mut();
    // SAFETY: The JVM invokes this native method with its current thread's
    // valid JNIEnv; GetJavaVM writes one VM pointer into the provided slot.
    let status = unsafe { ((**env).v1_2.GetJavaVM)(env, &mut jvm) };
    assert_eq!(status, uniffi_bindgen_kotlin_jni_runtime::JNI_OK);
    SHUTDOWN_JVM.store(jvm, Ordering::Release);
    bench_shutdown_wake_worker();
}

fn wake_sender() -> &'static mpsc::Sender<Option<Waker>> {
    &WAKE_WORKER
        .get_or_init(|| {
            let (sender, rx) = mpsc::channel::<Option<Waker>>();
            let thread = std::thread::Builder::new()
                .name("async-bench-wake".into())
                .spawn(move || {
                    while let Ok(Some(waker)) = rx.recv() {
                        waker.wake();
                    }
                    #[cfg(feature = "uniffi-bindgen-kotlin-jni")]
                    {
                        let jvm = SHUTDOWN_JVM.load(Ordering::Acquire);
                        assert!(!jvm.is_null(), "use the fixture JNI teardown entry");
                        // SAFETY: GetJavaVM supplied this VM, and the host is
                        // still inside bench_shutdown_wake_worker waiting to
                        // join us. Every wake call has returned, so this native
                        // worker has no Java frames or local JNI refs left.
                        // The baseline attaches this thread on wake but does
                        // not detach it; the fixture owns its thread teardown.
                        let status = unsafe { ((**jvm).v1_2.DetachCurrentThread)(jvm) };
                        assert_eq!(status, uniffi_bindgen_kotlin_jni_runtime::JNI_OK);
                    }
                })
                .unwrap();
            WakeWorker {
                sender,
                thread: Mutex::new(Some(thread)),
            }
        })
        .sender
}

/// One-shot process teardown, outside all timed operations. In JNI the worker
/// attaches to the JVM when waking Kotlin, so it must exit before the JVM exits.
#[uniffi::export]
pub fn bench_shutdown_wake_worker() {
    if let Some(worker) = WAKE_WORKER.get() {
        if let Some(thread) = worker.thread.lock().unwrap().take() {
            worker.sender.send(None).unwrap();
            thread.join().unwrap();
        }
    }
}

#[uniffi::export]
pub async fn bench_ready(value: u32) -> u32 {
    value
}

#[uniffi::export]
pub async fn bench_pending(value: u32) -> u32 {
    let mut first = true;
    poll_fn(|cx| {
        if first {
            first = false;
            wake_sender().send(Some(cx.waker().clone())).unwrap();
            Poll::Pending
        } else {
            Poll::Ready(value)
        }
    })
    .await
}

#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait BenchAsyncCallback: Send + Sync {
    async fn call(&self, value: u32) -> u32;
}

#[uniffi::export]
pub async fn bench_callback(callback: Arc<dyn BenchAsyncCallback>, value: u32) -> u32 {
    callback.call(value).await
}

#[derive(Default, uniffi::Object)]
pub struct BenchCancelProbe {
    polled: AtomicBool,
    dropped: AtomicBool,
}

#[uniffi::export]
impl BenchCancelProbe {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_polled(&self) -> bool {
        self.polled.load(Ordering::Acquire)
    }
    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }
}

struct CancelGuard(Arc<BenchCancelProbe>);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::Release);
    }
}

#[uniffi::export]
pub async fn bench_cancel(probe: Arc<BenchCancelProbe>) -> u32 {
    let _guard = CancelGuard(probe.clone());
    probe.polled.store(true, Ordering::Release);
    pending().await
}

// JNI has no existing Stream ABI. This feature is only built for Swift/JNA.
#[cfg(feature = "async-bench-streams")]
mod streams {
    use super::*;
    use std::pin::Pin;
    use uniffi::deps::futures_core::Stream;

    #[derive(Debug, thiserror::Error, uniffi::Error)]
    pub enum BenchStreamError {
        #[error("benchmark stream failure")]
        Failure,
    }

    struct Sequence {
        next: u32,
        count: u32,
        pending: bool,
        needs_wake: bool,
    }
    impl Stream for Sequence {
        type Item = Result<u32, BenchStreamError>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if self.next == self.count {
                return Poll::Ready(None);
            }
            if self.pending && self.needs_wake {
                self.needs_wake = false;
                wake_sender().send(Some(cx.waker().clone())).unwrap();
                return Poll::Pending;
            }
            let value = self.next;
            self.next += 1;
            self.needs_wake = true;
            Poll::Ready(Some(Ok(value)))
        }
    }

    #[uniffi::export]
    pub fn bench_output_stream(
        count: u32,
        pending: bool,
    ) -> uniffi::UniFfiStream<u32, BenchStreamError> {
        Box::pin(Sequence {
            next: 0,
            count,
            pending,
            needs_wake: true,
        })
    }

    #[uniffi::export]
    pub async fn bench_input_stream(
        mut events: uniffi::UniFfiInputStream<u32, BenchStreamError>,
    ) -> Result<u64, BenchStreamError> {
        let mut sum = 0;
        while let Some(value) = poll_fn(|cx| Pin::new(&mut events).poll_next(cx)).await {
            sum += u64::from(value?);
        }
        Ok(sum)
    }

    #[uniffi::export]
    pub fn bench_roundtrip_stream(
        events: uniffi::UniFfiInputStream<u32, BenchStreamError>,
    ) -> uniffi::UniFfiStream<u32, BenchStreamError> {
        Box::pin(events)
    }
}
#[cfg(feature = "async-bench-streams")]
pub use streams::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        task::{Context, Wake},
        time::Duration,
    };

    struct WakeSignal(mpsc::Sender<()>);
    impl Wake for WakeSignal {
        fn wake(self: Arc<Self>) {
            self.0.send(()).unwrap();
        }
    }

    #[test]
    fn pending_future_really_suspends_and_wakes() {
        let (tx, rx) = mpsc::channel();
        let waker = Waker::from(Arc::new(WakeSignal(tx)));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(bench_pending(43));
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(43));
    }

    #[cfg(feature = "async-bench-streams")]
    #[test]
    fn empty_and_single_item_streams_include_eof() {
        for count in [0, 1] {
            for pending in [false, true] {
                let (tx, rx) = mpsc::channel();
                let waker = Waker::from(Arc::new(WakeSignal(tx)));
                let mut cx = Context::from_waker(&waker);
                let mut stream = bench_output_stream(count, pending);
                for expected in 0..count {
                    if pending {
                        assert!(stream.as_mut().poll_next(&mut cx).is_pending());
                        rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    assert!(
                        matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(Some(Ok(value))) if value == expected)
                    );
                }
                assert!(matches!(
                    stream.as_mut().poll_next(&mut cx),
                    Poll::Ready(None)
                ));
            }
        }
    }
}
