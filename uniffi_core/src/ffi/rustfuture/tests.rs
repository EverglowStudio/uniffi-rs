use once_cell::sync::OnceCell;
use std::{
    future::Future,
    mem::ManuallyDrop,
    panic,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use super::*;
use crate::{test_util::TestError, Lift, RustBuffer, RustCallStatusCode};

// Sender/Receiver pair that we use for testing
struct Channel {
    result: Option<Result<Result<String, TestError>, LiftArgsError>>,
    waker: Option<Waker>,
}

struct Sender(Arc<Mutex<Channel>>);

impl Sender {
    fn wake(&self) {
        let inner = self.0.lock().unwrap();
        if let Some(waker) = &inner.waker {
            waker.wake_by_ref();
        }
    }

    fn send(&self, value: Result<String, TestError>) {
        let mut inner = self.0.lock().unwrap();
        if inner.result.replace(Ok(value)).is_some() {
            panic!("value already sent");
        }
        if let Some(waker) = &inner.waker {
            waker.wake_by_ref();
        }
    }

    fn send_lift_args_error(&self, arg_name: &'static str, error: anyhow::Error) {
        let mut inner = self.0.lock().unwrap();
        if inner
            .result
            .replace(Err(LiftArgsError { arg_name, error }))
            .is_some()
        {
            panic!("value already sent");
        }
        if let Some(waker) = &inner.waker {
            waker.wake_by_ref();
        }
    }
}

struct Receiver(Arc<Mutex<Channel>>);

impl Future for Receiver {
    type Output = Result<Result<String, TestError>, LiftArgsError>;

    fn poll(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<Result<String, TestError>, LiftArgsError>> {
        let mut inner = self.0.lock().unwrap();
        match inner.result.take() {
            Some(v) => Poll::Ready(v),
            None => {
                inner.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

// Create a sender and rust future that we can use for testing
fn channel() -> (Sender, Arc<RustFuture<RustBuffer>>) {
    let channel = Arc::new(Mutex::new(Channel {
        result: None,
        waker: None,
    }));
    let rust_future = RustFuture::new(Box::pin(Receiver(channel.clone())), crate::UniFfiTag);
    (Sender(channel), Arc::new(rust_future))
}

/// Poll a Rust future and get an OnceCell that's set when the continuation is called
fn poll(rust_future: &Arc<RustFuture<RustBuffer>>) -> Arc<OnceCell<RustFuturePoll>> {
    let cell = Arc::new(OnceCell::new());
    let handle = Arc::into_raw(cell.clone()) as u64;
    rust_future
        .clone()
        .poll(RustFutureContinuationBoundCallback {
            callback: poll_continuation,
            data: handle,
        });
    cell
}

extern "C" fn poll_continuation(data: u64, code: RustFuturePoll) {
    let cell = unsafe { Arc::from_raw(data as *const OnceCell<RustFuturePoll>) };
    cell.set(code).expect("Error setting OnceCell");
}

fn complete(rust_future: Arc<RustFuture<RustBuffer>>) -> (RustBuffer, RustCallStatus) {
    let mut out_status_code = RustCallStatus::default();
    let return_value = rust_future.complete(&mut out_status_code);
    (return_value, out_status_code)
}

#[test]
fn test_success() {
    let (sender, rust_future) = channel();

    // Test polling the rust future before it's ready
    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), None);
    sender.wake();
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));

    // Test polling the rust future when it's ready
    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), None);
    sender.send(Ok("All done".into()));
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));

    // Future polls should immediately return ready
    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));

    // Complete the future
    let (return_buf, call_status) = complete(rust_future);
    assert_eq!(call_status.code, RustCallStatusCode::Success);
    assert_eq!(
        <String as Lift<crate::UniFfiTag>>::try_lift(return_buf).unwrap(),
        "All done"
    );
}

#[test]
fn test_error() {
    let (sender, rust_future) = channel();

    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), None);
    sender.send(Err("Something went wrong".into()));
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));

    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));

    let (_, call_status) = complete(rust_future);
    assert_eq!(call_status.code, RustCallStatusCode::Error);
    assert_eq!(
        <TestError as Lift<crate::UniFfiTag>>::try_lift_from_rust_buffer(ManuallyDrop::into_inner(
            call_status.error_buf
        ))
        .unwrap(),
        TestError::from("Something went wrong"),
    )
}

#[test]
fn test_lift_args_error() {
    let (sender, rust_future) = channel();

    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), None);
    sender.send_lift_args_error("arg0", anyhow::anyhow!("Invalid handle"));
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));

    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));

    let (_, call_status) = complete(rust_future);
    assert_eq!(call_status.code, RustCallStatusCode::UnexpectedError);
    assert_eq!(
        <String as Lift<crate::UniFfiTag>>::try_lift(ManuallyDrop::into_inner(
            call_status.error_buf
        ))
        .unwrap(),
        "Failed to convert arg 'arg0':\nInvalid handle",
    )
}

// Once `complete` is called, the inner future should be released, even if wakers still hold a
// reference to the RustFuture
#[test]
fn test_cancel() {
    let (_sender, rust_future) = channel();

    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), None);
    rust_future.cancel();
    // Cancellation requests another poll; only that poll can confirm cleanup.
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));

    // Future polls should immediately invoke the callback with RustFuturePoll::Ready
    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));

    let (_, call_status) = complete(rust_future);
    assert_eq!(call_status.code, RustCallStatusCode::Cancelled);
}

// Once `free` is called, the inner future should be released, even if wakers still hold a
// reference to the RustFuture
#[test]
fn test_release_future() {
    let (sender, rust_future) = channel();
    // Create a weak reference to the channel to use to check if rust_future has dropped its
    // future.
    let channel_weak = Arc::downgrade(&sender.0);
    drop(sender);
    // Create an extra ref to rust_future, simulating a waker that still holds a reference to
    // it
    let rust_future2 = rust_future.clone();

    // Complete the rust future
    rust_future.free();
    // Even though rust_future is still alive, the channel shouldn't be
    assert!(Arc::strong_count(&rust_future2) > 0);
    assert_eq!(channel_weak.strong_count(), 0);
    assert!(channel_weak.upgrade().is_none());
}

// If `free` is called with a continuation still stored, we should call it them then.
//
// This shouldn't happen in practice, but it seems like good defensive programming
#[test]
fn test_complete_with_stored_continuation() {
    let (_sender, rust_future) = channel();

    let continuation_result = poll(&rust_future);
    rust_future.free();
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));
}

// Test what happens if we see a `wake()` call while we're polling the future.  This can
// happen, for example, with futures that are handled by a tokio thread pool.  We should
// schedule another poll of the future in this case.
#[test]
fn test_wake_during_poll() {
    let mut first_time = true;
    let future = std::future::poll_fn(move |ctx| {
        if first_time {
            first_time = false;
            // Wake the future while we are in the middle of polling it
            ctx.waker().wake_by_ref();
            Poll::Pending
        } else {
            // The second time we're polled, we're ready
            Poll::Ready(Ok("All done".to_owned()))
        }
    });
    let rust_future: Arc<RustFuture<RustBuffer>> =
        Arc::new(RustFuture::new(Box::pin(future), crate::UniFfiTag));
    let continuation_result = poll(&rust_future);
    // The continuation function should called immediately
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Wake));
    // A second poll should finish the future
    let continuation_result = poll(&rust_future);
    assert_eq!(continuation_result.get(), Some(&RustFuturePoll::Ready));
    let (return_buf, call_status) = complete(rust_future);
    assert_eq!(call_status.code, RustCallStatusCode::Success);
    assert_eq!(
        <String as Lift<crate::UniFfiTag>>::try_lift(return_buf).unwrap(),
        "All done"
    );
}

#[cfg(feature = "async-runtime")]
mod lifecycle {
    use super::*;
    use crate::async_runtime::current_context;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CleanupGuard {
        dropped: Arc<AtomicUsize>,
        cleaned: Arc<AtomicUsize>,
        gate: Option<futures::channel::oneshot::Receiver<()>>,
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.take().unwrap();
            let cleaned = self.cleaned.clone();
            if let Some(context) = current_context() {
                context
                    .defer_cleanup(async move {
                        let _ = gate.await;
                        cleaned.fetch_add(1, Ordering::SeqCst);
                    })
                    .unwrap();
            }
        }
    }

    fn guard() -> (CleanupGuard, futures::channel::oneshot::Sender<()>) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        (
            CleanupGuard {
                dropped: Arc::new(AtomicUsize::new(0)),
                cleaned: Arc::new(AtomicUsize::new(0)),
                gate: Some(receiver),
            },
            sender,
        )
    }

    fn observe<T>(future: &Arc<RustFuture<T>>) -> Arc<OnceCell<RustFuturePoll>> {
        let cell = Arc::new(OnceCell::new());
        future.clone().poll(RustFutureContinuationBoundCallback {
            callback: poll_continuation,
            data: Arc::into_raw(cell.clone()) as u64,
        });
        cell
    }

    fn observe_pending<T>(future: &Arc<RustFuture<T>>) -> Arc<OnceCell<RustFuturePoll>> {
        // Inserting cleanup into FuturesUnordered may schedule an extra poll.
        // Such Wake notifications are not terminal acknowledgements.
        for _ in 0..8 {
            let result = observe(future);
            if result.get() != Some(&RustFuturePoll::Wake) {
                return result;
            }
        }
        panic!("cleanup did not quiesce at its closed gate");
    }

    #[test]
    fn cancellation_waits_for_business_drop_and_cleanup() {
        let (resource, gate) = guard();
        let dropped = resource.dropped.clone();
        let cleaned = resource.cleaned.clone();
        let future = Arc::new(RustFuture::<u32>::new(
            async move {
                let _resource = resource;
                std::future::pending::<()>().await;
                Ok(7u32)
            },
            crate::UniFfiTag,
        ));
        let waiting = observe(&future);
        assert_eq!(waiting.get(), None);
        future.cancel();
        assert_eq!(waiting.get(), Some(&RustFuturePoll::Wake));
        let cleaning = observe_pending(&future);
        assert_eq!(
            cleaning.get(),
            None,
            "cancel is not a cleanup acknowledgement"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        gate.send(()).unwrap();
        assert_eq!(cleaning.get(), Some(&RustFuturePoll::Wake));
        assert_eq!(observe(&future).get(), Some(&RustFuturePoll::Ready));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        let mut status = RustCallStatus::default();
        assert_eq!(future.complete(&mut status), 0);
        assert_eq!(status.code, RustCallStatusCode::Cancelled);
        future.free();
    }

    #[test]
    fn cancellation_before_first_poll_drops_captures_in_context() {
        let (resource, gate) = guard();
        let cleaned = resource.cleaned.clone();
        let polled = Arc::new(AtomicUsize::new(0));
        let business_polled = polled.clone();
        let future = Arc::new(RustFuture::<u32>::new(
            async move {
                let _resource = resource;
                business_polled.fetch_add(1, Ordering::SeqCst);
                Ok(7u32)
            },
            crate::UniFfiTag,
        ));
        future.cancel();
        let cleaning = observe_pending(&future);
        assert_ne!(cleaning.get(), Some(&RustFuturePoll::Ready));
        assert_eq!(polled.load(Ordering::SeqCst), 0);
        gate.send(()).unwrap();
        assert_eq!(observe(&future).get(), Some(&RustFuturePoll::Ready));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        future.free();
    }

    struct Output {
        lowered: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Output {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    // This test type lowers to an owned scalar; counters observe when the
    // conversion consumes the Rust value, without creating an FFI allocation.
    unsafe impl LowerReturn<crate::UniFfiTag> for Output {
        type ReturnType = u32;
        fn lower_return(value: Self) -> Result<u32, crate::RustCallError> {
            value.lowered.fetch_add(1, Ordering::SeqCst);
            Ok(7)
        }
    }

    #[test]
    fn ready_keeps_rust_output_until_complete_and_late_cancel_loses() {
        let lowered = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let output = Output {
            lowered: lowered.clone(),
            dropped: dropped.clone(),
        };
        let future = Arc::new(RustFuture::<u32>::new(
            async move { Ok(output) },
            crate::UniFfiTag,
        ));
        assert_eq!(observe(&future).get(), Some(&RustFuturePoll::Ready));
        assert_eq!(
            lowered.load(Ordering::SeqCst),
            0,
            "poll must not transfer Rust ownership to FFI"
        );
        future.cancel();
        let mut status = RustCallStatus::default();
        assert_eq!(future.complete(&mut status), 7);
        assert_eq!(status.code, RustCallStatusCode::Success);
        assert_eq!(lowered.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        future.free();
    }

    #[test]
    fn ready_output_is_dropped_without_lowering_when_handle_is_freed() {
        let lowered = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let output = Output {
            lowered: lowered.clone(),
            dropped: dropped.clone(),
        };
        let future = Arc::new(RustFuture::<u32>::new(
            async move { Ok(output) },
            crate::UniFfiTag,
        ));
        assert_eq!(observe(&future).get(), Some(&RustFuturePoll::Ready));
        future.free();
        assert_eq!(lowered.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    struct CleanupOutput {
        _resource: CleanupGuard,
    }

    // No owning ABI value is created: the test observes the cleanup registered
    // by dropping a Rust output that was never delivered to foreign code.
    unsafe impl LowerReturn<crate::UniFfiTag> for CleanupOutput {
        type ReturnType = u32;
        fn lower_return(_value: Self) -> Result<u32, crate::RustCallError> {
            panic!("an abandoned output must not be lowered")
        }
    }

    #[test]
    fn nested_free_waits_for_unpolled_captures_and_undelivered_output_cleanup() {
        use crate::async_runtime::{CallContext, CancellationToken};

        for poll_before_free in [false, true] {
            let (resource, gate) = guard();
            let dropped = resource.dropped.clone();
            let cleaned = resource.cleaned.clone();
            let future = Arc::new(RustFuture::<u32>::new(
                async move {
                    Ok(CleanupOutput {
                        _resource: resource,
                    })
                },
                crate::UniFfiTag,
            ));
            if poll_before_free {
                assert_eq!(observe(&future).get(), Some(&RustFuturePoll::Ready));
            }
            let parent = CallContext::new(CancellationToken::new());
            let mut releasing = Box::pin(parent.run(async move { future.free() }));
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(releasing.as_mut().poll(&mut cx).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            assert!(releasing.as_mut().poll(&mut cx).is_pending());
            gate.send(()).unwrap();
            assert_eq!(releasing.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        }
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn tokio_runtime_covers_registered_cleanup_after_business_returns() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let seen = cleaned.clone();
        let future = Arc::new(RustFuture::<u32>::new_tokio(
            async move {
                current_context()
                    .unwrap()
                    .defer_cleanup(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        seen.fetch_add(1, Ordering::SeqCst);
                    })
                    .unwrap();
                Ok(7u32)
            },
            crate::UniFfiTag,
        ));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let next = observe(&future);
            while next.get().is_none() {
                assert!(
                    std::time::Instant::now() < until,
                    "Tokio cleanup did not wake the host"
                );
                std::thread::yield_now();
            }
            if next.get() == Some(&RustFuturePoll::Ready) {
                break;
            }
        }
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        let mut status = RustCallStatus::default();
        assert_eq!(future.complete(&mut status), 7);
        assert_eq!(status.code, RustCallStatusCode::Success);
        future.free();
    }
}
