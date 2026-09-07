//! Typed JNI adapter for the common call lifecycle. JNI conversion happens only
//! after `Run` has committed the result and finished registered cleanup.
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};
use uniffi::{
    async_runtime::{CallContext, CancellationToken, Cancelled, Run},
    deps::futures_util::FutureExt,
    Handle, RustFutureCallback, RustFuturePoll, Scheduler,
};

/// Recognize cancellation without folding owner failures into the cancel channel.
pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.is::<uniffi::async_runtime::Cancelled>()
        || error.is::<uniffi::StreamCancelled>()
        || matches!(
            error.downcast_ref::<uniffi::async_runtime::LocalError>(),
            Some(uniffi::async_runtime::LocalError::Cancelled)
        )
        || matches!(
            error.downcast_ref::<uniffi::async_runtime::TaskError>(),
            Some(uniffi::async_runtime::TaskError::Cancelled)
        )
}

type TypedFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>>;
struct State<T> {
    future: Option<TypedFuture<T>>,
    result: Option<anyhow::Result<T>>,
}
struct Continuation {
    callback: extern "C" fn(u64, RustFuturePoll),
    data: u64,
}
impl RustFutureCallback for Continuation {
    fn invoke(self, poll: RustFuturePoll) {
        (self.callback)(self.data, poll);
    }
}
struct JniFuture<T> {
    token: CancellationToken,
    scheduler: Scheduler<Continuation>,
    state: Mutex<State<T>>,
}
impl<T: Send> Wake for JniFuture<T> {
    fn wake(self: Arc<Self>) {
        self.scheduler.wake();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.scheduler.wake();
    }
}

pub fn future_new<T: Send + 'static>(
    future: impl Future<Output = Result<T, uniffi::LiftArgsError>> + Send + 'static,
) -> Handle {
    future_new_with_runtime(future, std::convert::identity)
}

/// Adapt the entire call, including cancellation and deferred cleanup, before
/// erasing its type. Generated scaffolding supplies the selected runtime adapter
/// (for example `uniffi::deps::async_compat::Compat::new`). This adds no box or task.
pub fn future_new_with_runtime<T, F, R>(future: F, adapt: impl FnOnce(Run<F>) -> R) -> Handle
where
    T: Send + 'static,
    F: Future<Output = Result<T, uniffi::LiftArgsError>> + Send + 'static,
    R: Future<Output = Result<Result<T, uniffi::LiftArgsError>, Cancelled>> + Send + 'static,
{
    let token = CancellationToken::new();
    let context = CallContext::new(token.clone());
    let run = adapt(context.run(future));
    let future = async move {
        match AssertUnwindSafe(run).catch_unwind().await {
            Ok(Ok(result)) => result.map_err(|e| e.error),
            Ok(Err(cancelled)) => Err(cancelled.into()),
            Err(_) => Err(anyhow::anyhow!("Rust future panicked")),
        }
    };
    Handle::from_arc(Arc::new(JniFuture {
        token,
        scheduler: Scheduler::new(),
        state: Mutex::new(State {
            future: Some(Box::pin(future)),
            result: None,
        }),
    }))
}

/// # Safety
/// Handle must be a live result of `future_new::<T>`; one poll may be in flight.
pub unsafe fn future_poll<T: Send + 'static>(
    handle: Handle,
    callback: extern "C" fn(u64, RustFuturePoll),
    data: u64,
) {
    let future = unsafe { Handle::into_arc_borrowed::<JniFuture<T>>(handle) };
    let ready = {
        let mut state = future.state.lock().unwrap();
        if state.result.is_some() {
            true
        } else {
            let waker = Waker::from(Arc::clone(&future));
            match state
                .future
                .as_mut()
                .expect("poll after complete")
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
            {
                Poll::Pending => false,
                Poll::Ready(result) => {
                    state.future = None;
                    state.result = Some(result);
                    true
                }
            }
        }
    };
    // Never invoke JVM callbacks under the future's state lock.
    let continuation = Continuation { callback, data };
    if ready {
        continuation.invoke(RustFuturePoll::Ready);
    } else {
        future.scheduler.store(continuation);
    }
}

/// # Safety
/// Handle must be a live result of `future_new::<T>`.
pub unsafe fn future_cancel<T: Send + 'static>(handle: Handle) {
    let future = unsafe { Handle::into_arc_borrowed::<JniFuture<T>>(handle) };
    future.token.cancel();
    future.scheduler.wake();
}

/// # Safety
/// Handle must be a live result of `future_new::<T>` which reported Ready.
/// Call exactly once, before free.
pub unsafe fn future_complete<T: Send + 'static>(handle: Handle) -> anyhow::Result<T> {
    let future = unsafe { Handle::into_arc_borrowed::<JniFuture<T>>(handle) };
    let result = future
        .state
        .lock()
        .unwrap()
        .result
        .take()
        .expect("complete before Ready");
    result
}

/// # Safety
/// Handle must be a live result of `future_new::<T>`. The caller must first wait
/// for Ready, drain the continuation and serialize free against cancel/poll.
pub unsafe fn future_free<T: Send + 'static>(handle: Handle) {
    drop(unsafe { Handle::into_arc::<JniFuture<T>>(handle) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use uniffi::async_runtime::{current_context, Cancelled};

    extern "C" fn notify(data: u64, poll: RustFuturePoll) {
        // SAFETY: each test keeps the AtomicUsize alive until all polls finish.
        let state = unsafe { &*std::ptr::with_exposed_provenance::<AtomicUsize>(data as usize) };
        state.store(
            if poll == RustFuturePoll::Ready { 2 } else { 1 },
            Ordering::SeqCst,
        );
    }
    struct CleanupProbe {
        dropped: Arc<AtomicBool>,
        gate: Option<uniffi::oneshot::Receiver<()>>,
        cleaned: Arc<AtomicBool>,
    }
    impl Drop for CleanupProbe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            let gate = self.gate.take().unwrap();
            let cleaned = self.cleaned.clone();
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    gate.await;
                    cleaned.store(true, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
    #[test]
    fn cancel_waits_for_real_cleanup_and_accepts_send_not_sync() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (gate, receiver) = uniffi::oneshot::channel();
        let cleaned = Arc::new(AtomicBool::new(false));
        let probe = CleanupProbe {
            dropped: dropped.clone(),
            gate: Some(receiver),
            cleaned: cleaned.clone(),
        };
        let send_not_sync = Cell::new(1u32);
        let handle = future_new(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
            Ok(send_not_sync.get())
        });
        let status = AtomicUsize::new(0);
        let data = std::ptr::from_ref(&status).expose_provenance() as u64;
        // SAFETY: one typed poll at a time, no access after free.
        unsafe {
            future_poll::<u32>(handle.clone(), notify, data);
            assert_eq!(status.load(Ordering::SeqCst), 0);
            future_cancel::<u32>(handle.clone());
            assert_eq!(status.load(Ordering::SeqCst), 1);
            status.store(0, Ordering::SeqCst);
            future_poll::<u32>(handle.clone(), notify, data);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(!cleaned.load(Ordering::SeqCst));
            assert_ne!(status.load(Ordering::SeqCst), 2);
            // Repeated cancellation cannot settle before the actual cleanup wake.
            future_cancel::<u32>(handle.clone());
            gate.send(());
            assert!(finish_polling::<u32>(handle, &status)
                .unwrap_err()
                .is::<Cancelled>());
            assert!(cleaned.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn ready_nullable_result_is_retained_without_lowering() {
        let handle = future_new(async { Ok(None::<u32>) });
        let status = AtomicUsize::new(0);
        let data = std::ptr::from_ref(&status).expose_provenance() as u64;
        unsafe {
            future_poll::<Option<u32>>(handle.clone(), notify, data);
            assert_eq!(status.load(Ordering::SeqCst), 2);
            assert_eq!(
                future_complete::<Option<u32>>(handle.clone()).unwrap(),
                None
            );
            future_free::<Option<u32>>(handle);
        }
    }

    // Continue the actual host protocol: a stored poll must wake before the next
    // poll. Never create a Tokio runtime on this host test thread.
    unsafe fn finish_polling<T: Send + 'static>(
        handle: Handle,
        status: &AtomicUsize,
    ) -> anyhow::Result<T> {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let data = std::ptr::from_ref(status).expose_provenance() as u64;
        loop {
            assert!(
                std::time::Instant::now() < until,
                "cleanup did not wake host"
            );
            match status.load(Ordering::SeqCst) {
                2 => break,
                1 => {
                    status.store(0, Ordering::SeqCst);
                    unsafe { future_poll::<T>(handle.clone(), notify, data) };
                }
                _ => std::thread::yield_now(),
            }
        }
        let result = unsafe { future_complete::<T>(handle.clone()) };
        unsafe { future_free::<T>(handle) };
        result
    }

    struct RuntimeCleanup {
        gate: Option<uniffi::oneshot::Receiver<()>>,
        dropped: Arc<AtomicBool>,
        cleaned: Arc<AtomicBool>,
        tokio: bool,
    }
    impl Drop for RuntimeCleanup {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            let gate = self.gate.take().unwrap();
            let cleaned = self.cleaned.clone();
            let tokio = self.tokio;
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    // Construct the timer only after this gate has forced Run
                    // to return Pending after the business future was dropped.
                    gate.await;
                    assert_eq!(tokio::runtime::Handle::try_current().is_ok(), tokio);
                    if tokio {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    cleaned.store(true, Ordering::SeqCst);
                })
                .unwrap();
        }
    }

    fn runtime_cleanup_case(tokio: bool, cancel: bool) {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let (gate, receiver) = uniffi::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let cleaned = Arc::new(AtomicBool::new(false));
        let probe = RuntimeCleanup {
            gate: Some(receiver),
            dropped: dropped.clone(),
            cleaned: cleaned.clone(),
            tokio,
        };
        let business = async move {
            assert_eq!(tokio::runtime::Handle::try_current().is_ok(), tokio);
            let probe = probe;
            if cancel {
                std::future::pending::<()>().await;
            }
            drop(probe);
            Ok(7u32)
        };
        let handle = if tokio {
            future_new_with_runtime(business, uniffi::deps::async_compat::Compat::new)
        } else {
            future_new(business)
        };
        let status = AtomicUsize::new(0);
        let data = std::ptr::from_ref(&status).expose_provenance() as u64;
        // SAFETY: typed, serialized polling, callback data outlives completion;
        // finish_polling drains Ready and frees exactly once.
        unsafe {
            future_poll::<u32>(handle.clone(), notify, data);
            if cancel {
                assert!(!dropped.load(Ordering::SeqCst));
                future_cancel::<u32>(handle.clone());
                assert_eq!(status.load(Ordering::SeqCst), 1);
                status.store(0, Ordering::SeqCst);
                future_poll::<u32>(handle.clone(), notify, data);
            }
            assert!(dropped.load(Ordering::SeqCst));
            assert!(!cleaned.load(Ordering::SeqCst));
            assert_ne!(status.load(Ordering::SeqCst), 2);
            assert!(tokio::runtime::Handle::try_current().is_err());
            gate.send(());
            let result = finish_polling::<u32>(handle, &status);
            assert!(cleaned.load(Ordering::SeqCst));
            if cancel {
                assert!(result.unwrap_err().is::<Cancelled>());
            } else {
                assert_eq!(result.unwrap(), 7);
            }
        }
        assert!(tokio::runtime::Handle::try_current().is_err());
    }

    #[test]
    fn tokio_covers_cleanup_timer_after_business_returns() {
        runtime_cleanup_case(true, false);
    }

    #[test]
    fn tokio_covers_cleanup_timer_after_cancellation() {
        runtime_cleanup_case(true, true);
    }

    #[test]
    fn ordinary_call_and_cleanup_do_not_enter_tokio() {
        runtime_cleanup_case(false, false);
        runtime_cleanup_case(false, true);
    }
}
