//! Monotonic timers and cleanup-aware deadlines.

use std::{
    fmt,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
    time::Duration,
};

use futures_util::future::poll_fn;

use super::{current_context, CallContext, CancellationToken, SendOutsideWasm};

#[cfg(not(target_arch = "wasm32"))]
use tokio::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

// gloo 0.3 casts its u32 milliseconds to the signed JS timer argument. This
// bound also keeps native deadlines comfortably inside Tokio's timer range.
const MAX_TIMER_CHUNK: Duration = Duration::from_millis(i32::MAX as u64);

/// Wait for a duration, measured from the first poll using a monotonic clock.
///
/// Zero completes immediately. All durations, including [`Duration::MAX`], are
/// supported by scheduling bounded chunks. Dropping the future clears its timer.
///
/// # Panics
///
/// On native, polling a nonzero sleep requires a current Tokio runtime with
/// timers enabled. On WASM, the current JS global must provide timers and a
/// monotonic performance clock.
pub async fn sleep(duration: Duration) {
    let mut remaining = duration;
    while !remaining.is_zero() {
        let chunk = remaining.min(MAX_TIMER_CHUNK);
        let started = Instant::now();
        #[cfg(not(target_arch = "wasm32"))]
        tokio::time::sleep(chunk).await;
        #[cfg(target_arch = "wasm32")]
        gloo_timers::future::TimeoutFuture::new(timer_millis(chunk)).await;
        // Account for late wakes (including a suspended page) and avoid adding
        // the whole duration to Instant, which can overflow for Duration::MAX.
        remaining = remaining.saturating_sub(started.elapsed());
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn timer_millis(chunk: Duration) -> u32 {
    // Round sub-millisecond durations up, so a timer never intentionally fires
    // early. The chunk bound ensures rounding cannot overflow the JS argument.
    chunk.as_nanos().div_ceil(1_000_000) as u32
}

/// The operation did not complete before its deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation timed out")
    }
}

impl std::error::Error for Elapsed {}

// A completed timeout has already driven its cleanup. Only an interrupted
// timeout needs to transfer that responsibility to the enclosing call. Eagerly
// registering every child would retain completed contexts for the entire
// lifetime of a long-running parent loop.
struct TimeoutCleanup(Option<CallContext>);

impl Drop for TimeoutCleanup {
    fn drop(&mut self) {
        if let (Some(child), Some(parent)) = (self.0.take(), current_context()) {
            parent
                .defer_cleanup(async move {
                    child.wait_closed().await.expect("timeout cleanup panicked");
                })
                .expect("current call context must accept child cleanup");
        }
    }
}

/// Run a future with a deadline and wait for its registered cleanup.
///
/// The operation is polled before the timer, including for a zero deadline.
/// Once it returns a value, its cleanup may finish after the deadline without
/// changing the result. On timeout the operation is dropped before its cleanup
/// is awaited. A surrounding call context also waits for this child's cleanup
/// if this timeout is dropped. Without a surrounding context, callers needing
/// that confirmation must retain and drive their own [`CallContext`].
///
/// The deadline starts on first poll. Like all cooperative deadlines, it cannot
/// interrupt a poll that does not return. Timer requirements match [`sleep`].
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, Elapsed>
where
    F: Future + SendOutsideWasm,
    F::Output: SendOutsideWasm,
{
    let started = Instant::now();
    let child = CallContext::new(CancellationToken::new());
    let mut cleanup_on_drop = TimeoutCleanup(Some(child.clone()));
    let completed = AtomicBool::new(false);
    let completed_ref = &completed;
    let mut run = Box::pin(child.run(async move {
        let output = future.await;
        completed_ref.store(true, Ordering::Relaxed);
        output
    }));
    let mut timer = Some(Box::pin(async {
        sleep(duration.saturating_sub(started.elapsed())).await;
    }));
    let result = poll_fn(|cx| {
        if let Poll::Ready(result) = run.as_mut().poll(cx) {
            return Poll::Ready(result.map_err(|_| Elapsed));
        }
        if completed.load(Ordering::Relaxed) {
            // Cleanup may be pending, but the business deadline is now settled.
            timer.take();
        } else if timer.as_mut().unwrap().as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed));
        }
        Poll::Pending
    })
    .await;
    drop(timer);
    // A business destructor may panic on timeout. Still finish the cleanup it
    // registered before propagating that panic, even without a parent context.
    let dropped = catch_unwind(AssertUnwindSafe(|| drop(run)));
    let cleanup = child.wait_closed().await;
    cleanup_on_drop.0 = None;
    if let Err(panic) = dropped {
        resume_unwind(panic);
    }
    cleanup.expect("timeout cleanup panicked");
    result
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use futures_channel::oneshot;
    use futures_util::{future::pending, poll};
    use std::sync::{atomic::AtomicUsize, Arc};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    struct CleanupOnDrop {
        dropped: Arc<AtomicUsize>,
        cleaned: Arc<AtomicUsize>,
        gate: Option<oneshot::Receiver<()>>,
    }

    impl Drop for CleanupOnDrop {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.take().unwrap();
            let cleaned = self.cleaned.clone();
            current_context()
                .expect("child business destructor must have its context")
                .defer_cleanup(async move {
                    gate.await.unwrap();
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }

    fn resource() -> (
        CleanupOnDrop,
        oneshot::Sender<()>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        (
            CleanupOnDrop {
                dropped: dropped.clone(),
                cleaned: cleaned.clone(),
                gate: Some(receiver),
            },
            sender,
            dropped,
            cleaned,
        )
    }

    #[test]
    fn zero_sleep_and_ready_timeout_need_no_runtime() {
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(Box::pin(sleep(Duration::ZERO))
            .as_mut()
            .poll(&mut cx)
            .is_ready());
        assert_eq!(
            Box::pin(timeout(Duration::ZERO, async { 7 }))
                .as_mut()
                .poll(&mut cx),
            Poll::Ready(Ok(7))
        );
    }

    #[test]
    fn timer_rounding_and_chunk_boundary() {
        assert_eq!(timer_millis(Duration::ZERO), 0);
        assert_eq!(timer_millis(Duration::from_nanos(1)), 1);
        assert_eq!(timer_millis(Duration::from_micros(1_001)), 2);
        assert_eq!(timer_millis(MAX_TIMER_CHUNK), i32::MAX as u32);
        assert_eq!(Duration::MAX.min(MAX_TIMER_CHUNK), MAX_TIMER_CHUNK);
    }

    #[test]
    fn max_sleep_stays_pending_and_can_be_dropped() {
        runtime().block_on(async {
            let mut future = Box::pin(sleep(Duration::MAX));
            assert!(poll!(&mut future).is_pending());
            drop(future);
            let started = Instant::now();
            sleep(Duration::from_micros(100)).await;
            assert!(started.elapsed() >= Duration::from_micros(100));
        });
    }

    #[test]
    fn timeout_drops_business_before_waiting_for_cleanup() {
        runtime().block_on(async {
            let (resource, gate, dropped, cleaned) = resource();
            let mut future = Box::pin(timeout(Duration::ZERO, async move {
                let _resource = resource;
                pending::<()>().await;
            }));
            assert!(poll!(&mut future).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            gate.send(()).unwrap();
            assert_eq!(future.await, Err(Elapsed));
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn completed_business_wins_even_while_cleanup_is_pending() {
        runtime().block_on(async {
            let (resource, gate, dropped, cleaned) = resource();
            let mut future = Box::pin(timeout(Duration::ZERO, async move {
                let _resource = resource;
                42
            }));
            assert!(poll!(&mut future).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            assert!(poll!(&mut future).is_pending());
            gate.send(()).unwrap();
            assert_eq!(future.await, Ok(42));
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn outer_drop_transfers_child_cleanup_to_parent() {
        runtime().block_on(async {
            let parent = CallContext::new(CancellationToken::new());
            let (resource, gate, dropped, cleaned) = resource();
            let mut outer = Box::pin(parent.run(timeout(Duration::MAX, async move {
                let _resource = resource;
                pending::<()>().await;
            })));
            assert!(poll!(&mut outer).is_pending());
            drop(outer);
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            let mut closed = Box::pin(parent.wait_closed());
            assert!(poll!(&mut closed).is_pending());
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            gate.send(()).unwrap();
            assert_eq!(closed.await, Ok(()));
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn timeout_accepts_borrowed_send_but_not_sync_future_and_output() {
        runtime().block_on(async {
            let mut value = std::cell::Cell::new(3);
            let result = timeout(Duration::ZERO, async {
                let value = &mut value;
                std::cell::Cell::new(value.replace(4))
            })
            .await
            .unwrap();
            assert_eq!(result.get(), 3);
            assert_eq!(value.get(), 4);
        });
    }

    #[test]
    fn cleanup_panic_is_observable() {
        use futures_util::FutureExt;
        runtime().block_on(async {
            let result = AssertUnwindSafe(timeout(Duration::ZERO, async {
                current_context()
                    .unwrap()
                    .defer_cleanup(async { panic!("cleanup") })
                    .unwrap();
                pending::<()>().await;
            }))
            .catch_unwind()
            .await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn destructor_panic_still_waits_for_cleanup_without_parent() {
        use futures_util::FutureExt;
        struct PanicOnDrop;
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("business destructor");
            }
        }
        runtime().block_on(async {
            let (resource, gate, dropped, cleaned) = resource();
            let mut future = Box::pin(
                AssertUnwindSafe(timeout(Duration::ZERO, async move {
                    let _panic = PanicOnDrop;
                    let _resource = resource;
                    pending::<()>().await;
                }))
                .catch_unwind(),
            );
            assert!(poll!(&mut future).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            gate.send(()).unwrap();
            assert!(future.await.is_err());
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        });
    }
}
