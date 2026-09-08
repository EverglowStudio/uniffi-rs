use super::{BoxFuture, SendOutsideWasm};
use futures_channel::oneshot;
use futures_util::{
    future::Shared, stream::FuturesUnordered, task::AtomicWaker, FutureExt, StreamExt,
};
use std::{
    any::Any,
    cell::RefCell,
    fmt,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll},
};
use tokio::sync::Notify;

#[cfg(target_arch = "wasm32")]
use std::rc::{Rc as ContextRef, Weak};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc as ContextRef, Weak};

type Signal = Shared<oneshot::Receiver<()>>;
type CancellationSignal = BoxFuture<'static, ()>;
type Cleanup = Shared<BoxFuture<'static, Result<(), CleanupFailed>>>;
type Panic = Box<dyn Any + Send>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A cancellation request. Cancellation is not proof that a call has closed.
#[derive(Clone)]
pub struct CancellationToken {
    state: Arc<TokenState>,
}
struct TokenState {
    cancelled: AtomicBool,
    notification: Notify,
}
struct Notification {
    sender: Option<oneshot::Sender<()>>,
    signal: Signal,
}
impl Notification {
    fn new() -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            sender: Some(sender),
            signal: receiver.shared(),
        }
    }
}
impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
impl CancellationToken {
    pub fn new() -> Self {
        Self {
            state: Arc::new(TokenState {
                cancelled: AtomicBool::new(false),
                notification: Notify::new(),
            }),
        }
    }
    /// Request cancellation, idempotently. Wakes all registered waiters.
    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.notification.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }
    pub async fn cancelled(&self) {
        // Tokio 1.38 records notify_waiters broadcasts from Notified creation,
        // even before its first poll. Create it before checking the flag so a
        // concurrent cancel is covered by either the flag or the broadcast.
        let notified = self.state.notification.notified();
        if !self.is_cancelled() {
            notified.await;
        }
    }
    fn signal(&self) -> Option<CancellationSignal> {
        if self.is_cancelled() {
            return None;
        }
        // Own the token inside the pinned async future: Tokio 1.38 has no
        // OwnedNotified, and Run remains movable without pinning its fields.
        let token = self.clone();
        Some(Box::pin(async move { token.cancelled().await }))
    }
}
impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupClosed;
/// At least one registered cleanup future panicked. Remaining cleanup was drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupFailed;
macro_rules! error {
    ($ty:ty, $message:literal) => {
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str($message)
            }
        }
        impl std::error::Error for $ty {}
    };
}
error!(Cancelled, "call cancelled");
error!(CleanupClosed, "call cleanup is already closed");
error!(CleanupFailed, "a registered cleanup panicked");

#[derive(Clone)]
pub struct CallContext {
    inner: ContextRef<Inner>,
}
struct Inner {
    token: CancellationToken,
    // The single-run gate does not publish cleanup or closed state; those stay
    // protected by state. Competing run callers only need an atomic winner.
    started: AtomicBool,
    state: Mutex<CallState>,
    cleanup_waker: AtomicWaker,
}
struct CallState {
    closed: Option<Result<(), CleanupFailed>>,
    cleanup: Vec<BoxFuture<'static, ()>>,
    closing: Option<Cleanup>,
    notification: Option<Notification>,
}
thread_local! {
    static CURRENT: RefCell<Option<CallContext>> = const { RefCell::new(None) };
}
/// The context of the current business/cleanup poll or destructor, if any.
pub fn current_context() -> Option<CallContext> {
    CURRENT.with(|slot| slot.borrow().clone())
}
// Owner management is outside the caller's business context. Keep this a
// synchronous scope: the child Run installs its own context for poll and Drop.
pub(super) fn without_current<T>(f: impl FnOnce() -> T) -> T {
    let _scope = Scope(CURRENT.with(|slot| slot.replace(None)));
    f()
}
struct Scope(Option<CallContext>);
impl Scope {
    fn enter(context: &CallContext) -> Self {
        Self(CURRENT.with(|slot| slot.replace(Some(context.clone()))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

impl CallContext {
    pub fn new(token: CancellationToken) -> Self {
        Self {
            inner: ContextRef::new(Inner {
                token,
                started: AtomicBool::new(false),
                state: Mutex::new(CallState {
                    closed: None,
                    cleanup: Vec::new(),
                    closing: None,
                    notification: None,
                }),
                cleanup_waker: AtomicWaker::new(),
            }),
        }
    }
    /// Request cancellation; await the call or wait_closed for release confirmation.
    pub fn cancel(&self) {
        self.inner.token.cancel();
    }
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.token.clone()
    }
    /// Register asynchronous release confirmation, including from a destructor.
    ///
    /// Registration is accepted until cleanup has finished. Registered futures
    /// run concurrently after the business future is dropped and may themselves
    /// register cleanup. The returned error means registration was too late.
    pub fn defer_cleanup<F>(&self, future: F) -> Result<(), CleanupClosed>
    where
        F: Future<Output = ()> + SendOutsideWasm + 'static,
    {
        {
            let mut state = lock(&self.inner.state);
            if state.closed.is_some() {
                return Err(CleanupClosed);
            }
            state.cleanup.push(Box::pin(future));
        }
        self.inner.cleanup_waker.wake();
        Ok(())
    }
    /// Start this context's single call. A second call on the same context panics.
    /// Cancellation before the first poll never polls the business future.
    pub fn run<F: Future>(&self, future: F) -> Run<F> {
        assert!(
            !self.inner.started.swap(true, Ordering::AcqRel),
            "a CallContext can run only one call"
        );
        Run {
            context: self.clone(),
            future: Some(Box::pin(future)),
            cancellation: None,
            result: None,
            panic: None,
            cleanup: None,
            done: false,
        }
    }
    /// Wait for business drop and drive all registered asynchronous cleanup.
    ///
    /// Multiple waiters share the same cleanup work. Cancelling/dropping one
    /// waiter does not discard work; another waiter can resume it. Before `run`
    /// has been dropped or finished this waits for that call, without cancelling it.
    /// A panic is reported only after the other cleanup futures finish.
    pub async fn wait_closed(&self) -> Result<(), CleanupFailed> {
        loop {
            let (cleanup, notification) = {
                let mut state = lock(&self.inner.state);
                if let Some(result) = state.closed {
                    return result;
                }
                if let Some(cleanup) = &state.closing {
                    (Some(cleanup.clone()), None)
                } else {
                    (
                        None,
                        Some(
                            state
                                .notification
                                .get_or_insert_with(Notification::new)
                                .signal
                                .clone(),
                        ),
                    )
                }
            };
            if let Some(cleanup) = cleanup {
                return cleanup.await;
            }
            let _ = notification.unwrap().await;
        }
    }
    fn begin_close(&self) -> Option<Cleanup> {
        let (cleanup, sender) = {
            let mut state = lock(&self.inner.state);
            if state.closed.is_some() {
                return None;
            }
            if let Some(cleanup) = &state.closing {
                return Some(cleanup.clone());
            }
            // Business Drop has already run in scope. Closing and registration
            // use the same lock, so a concurrent registration is either driven
            // below or rejected after the empty call is atomically closed.
            let cleanup = if state.cleanup.is_empty() {
                state.closed = Some(Ok(()));
                None
            } else {
                let driver = CleanupDriver {
                    inner: ContextRef::downgrade(&self.inner),
                    pending: FuturesUnordered::new(),
                    failed: false,
                };
                let cleanup: BoxFuture<'static, Result<(), CleanupFailed>> = Box::pin(driver);
                let cleanup = cleanup.shared();
                state.closing = Some(cleanup.clone());
                Some(cleanup)
            };
            let sender = state
                .notification
                .as_mut()
                .and_then(|notification| notification.sender.take());
            (cleanup, sender)
        };
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        cleanup
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod retention_tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Barrier};

    #[test]
    fn concurrent_run_has_one_winner_and_preserves_cleanup() {
        let context = CallContext::new(CancellationToken::new());
        let entered = Arc::new(Barrier::new(8));
        let polls = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cleaned = cleanups.clone();
        context
            .defer_cleanup(async move {
                cleaned.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let context = context.clone();
                let entered = entered.clone();
                let polls = polls.clone();
                std::thread::spawn(move || {
                    entered.wait();
                    match catch_unwind(AssertUnwindSafe(|| {
                        context.run(async move {
                            polls.fetch_add(1, Ordering::SeqCst);
                        })
                    })) {
                        Ok(call) => {
                            assert_eq!(futures::executor::block_on(call), Ok(()));
                            true
                        }
                        Err(panic) => {
                            let message = panic
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
                            assert_eq!(message, Some("a CallContext can run only one call"));
                            false
                        }
                    }
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
    }

    struct DropContextProbe {
        expected: CallContext,
        observed: Arc<Mutex<Vec<bool>>>,
    }
    impl Drop for DropContextProbe {
        fn drop(&mut self) {
            let in_context = current_context()
                .is_some_and(|current| ContextRef::ptr_eq(&current.inner, &self.expected.inner));
            // Assert after catching any surrounding panic, not in a destructor
            // that might itself already be running during unwinding.
            lock(&self.observed).push(in_context);
        }
    }

    #[test]
    fn cleanup_panic_keeps_unreturned_result_drop_in_context() {
        let context = CallContext::new(CancellationToken::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let value = DropContextProbe {
            expected: context.clone(),
            observed: observed.clone(),
        };
        context
            .defer_cleanup(async { panic!("cleanup failed") })
            .unwrap();
        let mut call = context.run(futures::future::ready(value));
        let failure = catch_unwind(AssertUnwindSafe(|| futures::executor::block_on(&mut call)));
        assert!(failure.is_err());
        assert!(lock(&observed).is_empty());
        drop(call);
        assert_eq!(*lock(&observed), vec![true]);
        assert!(current_context().is_none());
        assert_eq!(
            futures::executor::block_on(context.wait_closed()),
            Err(CleanupFailed)
        );
    }

    #[test]
    fn ready_business_drop_panic_drains_cleanup_and_scopes_unreturned_result() {
        struct PanicAfterReady {
            value: Option<DropContextProbe>,
            _business_drop: DropContextProbe,
        }
        impl Future for PanicAfterReady {
            type Output = DropContextProbe;
            fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(self.value.take().unwrap())
            }
        }
        impl Drop for PanicAfterReady {
            fn drop(&mut self) {
                panic!("business destructor failed after Ready");
            }
        }

        let context = CallContext::new(CancellationToken::new());
        let business_drop = Arc::new(Mutex::new(Vec::new()));
        let result_drop = Arc::new(Mutex::new(Vec::new()));
        let (release, released) = oneshot::channel();
        context
            .defer_cleanup(async move { released.await.unwrap() })
            .unwrap();
        let mut call = context.run(PanicAfterReady {
            value: Some(DropContextProbe {
                expected: context.clone(),
                observed: result_drop.clone(),
            }),
            _business_drop: DropContextProbe {
                expected: context.clone(),
                observed: business_drop.clone(),
            },
        });
        let waker = futures::task::noop_waker();
        assert!(Pin::new(&mut call)
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        assert_eq!(*lock(&business_drop), vec![true]);
        assert!(lock(&result_drop).is_empty());
        assert!(current_context().is_none());
        release.send(()).unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| {
            futures::executor::block_on(&mut call)
        }))
        .is_err());
        drop(call);
        assert_eq!(*lock(&result_drop), vec![true]);
        assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
        assert!(current_context().is_none());
    }

    #[test]
    fn completed_run_drop_preserves_outer_context_and_delivered_value() {
        let inner = CallContext::new(CancellationToken::new());
        let outer = CallContext::new(CancellationToken::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let value = DropContextProbe {
            expected: outer.clone(),
            observed: observed.clone(),
        };
        let mut completed = inner.run(futures::future::ready(value));
        let delivered = futures::executor::block_on(&mut completed).unwrap();
        let expected_outer = outer.clone();
        futures::executor::block_on(outer.run(async move {
            drop(completed);
            assert!(ContextRef::ptr_eq(
                &current_context().unwrap().inner,
                &expected_outer.inner
            ));
            assert!(lock(&observed).is_empty());
            assert_eq!(inner.wait_closed().await, Ok(()));
            drop(delivered);
            assert_eq!(*lock(&observed), vec![true]);
        }))
        .unwrap();
        assert!(current_context().is_none());
    }

    #[test]
    fn ready_call_and_late_waiters_allocate_no_cancellation_waiter_or_close_driver() {
        let token = CancellationToken::new();
        let context = CallContext::new(token.clone());
        let mut call = context.run(async { 42 });
        assert!(call.cancellation.is_none());
        assert!(lock(&context.inner.state).notification.is_none());
        assert_eq!(futures::executor::block_on(&mut call), Ok(42));
        assert!(call.cancellation.is_none());
        futures::executor::block_on(async {
            let (one, two) = futures::join!(context.wait_closed(), context.wait_closed());
            assert_eq!((one, two), (Ok(()), Ok(())));
        });
        let state = lock(&context.inner.state);
        assert_eq!(state.closed, Some(Ok(())));
        assert!(state.notification.is_none());
        assert!(state.closing.is_none());
        assert_eq!(Arc::strong_count(&token.state), 2);
    }

    #[test]
    fn precancelled_and_unpolled_drop_need_no_notification_or_driver() {
        for cancelled in [false, true] {
            let token = CancellationToken::new();
            let context = CallContext::new(token.clone());
            let mut call = context.run(async { panic!("business must not run") });
            assert!(call.cancellation.is_none());
            if cancelled {
                token.cancel();
                assert_eq!(futures::executor::block_on(&mut call), Err(Cancelled));
                assert!(call.cancellation.is_none());
            }
            drop(call);
            assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
            assert_eq!(Arc::strong_count(&token.state), 2);
            let state = lock(&context.inner.state);
            assert!(state.notification.is_none());
            assert!(state.closing.is_none());
        }
    }

    #[test]
    fn early_close_waiters_share_lazy_notification_and_survive_waiter_drop() {
        let context = CallContext::new(CancellationToken::new());
        let mut one = Box::pin(context.wait_closed());
        let mut two = Box::pin(context.wait_closed());
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(one.as_mut().poll(&mut cx).is_pending());
        assert!(two.as_mut().poll(&mut cx).is_pending());
        assert!(lock(&context.inner.state).notification.is_some());
        drop(one);
        assert_eq!(futures::executor::block_on(context.run(async { 7 })), Ok(7));
        assert_eq!(futures::executor::block_on(two), Ok(()));
        assert!(lock(&context.inner.state).closing.is_none());
    }

    #[test]
    fn cancellation_during_first_pending_poll_cannot_lose_lazy_wakeup() {
        for _ in 0..64 {
            let token = CancellationToken::new();
            let context = CallContext::new(token.clone());
            let (entered, observed) = std::sync::mpsc::sync_channel(0);
            let worker = std::thread::spawn(move || {
                let mut entered = Some(entered);
                futures::executor::block_on(context.run(futures::future::poll_fn(move |_| {
                    if let Some(entered) = entered.take() {
                        entered.send(()).unwrap();
                    }
                    Poll::<()>::Pending
                })))
            });
            observed.recv().unwrap();
            token.cancel();
            assert_eq!(worker.join().unwrap(), Err(Cancelled));
        }
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancellation_wakes_all_live_waiters_once_and_accepts_late_waiters() {
        let token = CancellationToken::new();
        let counts: Vec<_> = (0..8).map(|_| Arc::new(WakeCount::default())).collect();
        let mut waiters: Vec<_> = counts
            .iter()
            .map(|count| {
                let waker = std::task::Waker::from(count.clone());
                let mut waiter = Box::pin(token.cancelled());
                assert!(waiter
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending());
                waiter
            })
            .collect();
        drop(waiters.remove(3));
        let unpolled = token.signal().unwrap();
        token.cancel();
        token.cancel();
        for (index, count) in counts.iter().enumerate() {
            assert_eq!(count.0.load(Ordering::SeqCst), usize::from(index != 3));
        }
        let waker = futures::task::noop_waker();
        for mut waiter in waiters {
            assert!(waiter
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready());
        }
        assert!(Box::pin(unpolled)
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready());
        assert!(Box::pin(token.cancelled())
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready());
        assert!(token.signal().is_none());
    }

    #[test]
    fn broadcast_between_notified_creation_and_poll_is_remembered() {
        let token = CancellationToken::new();
        let notified = token.state.notification.notified();
        assert!(!token.is_cancelled());
        token.cancel();
        let waker = futures::task::noop_waker();
        assert!(Box::pin(notified)
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready());
    }

    #[test]
    fn dropping_owned_cancellation_waiter_releases_token_state() {
        let token = CancellationToken::new();
        let weak = Arc::downgrade(&token.state);
        let mut signal = token.signal().unwrap();
        let waker = futures::task::noop_waker();
        assert!(signal
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(token);
        assert!(weak.upgrade().is_some());
        drop(signal);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn pending_run_updates_its_cancellation_waker_and_stays_unpin() {
        fn require_unpin<T: Unpin>(_: &T) {}
        fn require_send_sync<T: Send + Sync>(_: &T) {}
        let token = CancellationToken::new();
        require_send_sync(&token.cancelled());
        let context = CallContext::new(token.clone());
        let mut call = context.run(futures::future::pending::<()>());
        require_unpin(&call);
        let old = Arc::new(WakeCount::default());
        let latest = Arc::new(WakeCount::default());
        for count in [&old, &latest] {
            let waker = std::task::Waker::from(count.clone());
            assert!(Pin::new(&mut call)
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
        }
        token.cancel();
        assert_eq!(old.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
        assert_eq!(futures::executor::block_on(call), Err(Cancelled));
        assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
    }

    #[test]
    fn repeated_completed_timeouts_do_not_retain_children_in_live_parent() {
        futures::executor::block_on(async {
            let parent = CallContext::new(CancellationToken::new());
            parent
                .run(async {
                    for _ in 0..128 {
                        let child =
                            crate::async_runtime::timeout(std::time::Duration::ZERO, async {
                                let current = current_context().unwrap();
                                ContextRef::downgrade(&current.inner)
                            })
                            .await
                            .unwrap();
                        assert!(
                            child.upgrade().is_none(),
                            "completed timeout retained by live parent"
                        );
                    }
                })
                .await
                .unwrap();
        });
    }
}

struct CleanupDriver {
    inner: Weak<Inner>,
    pending: FuturesUnordered<BoxFuture<'static, bool>>,
    failed: bool,
}
impl Future for CleanupDriver {
    type Output = Result<(), CleanupFailed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(inner) = self.inner.upgrade() else {
            return Poll::Ready(Err(CleanupFailed));
        };
        let context = CallContext { inner };
        let _scope = Scope::enter(&context);
        context.inner.cleanup_waker.register(cx.waker());
        loop {
            let queued = std::mem::take(&mut lock(&context.inner.state).cleanup);
            for future in queued {
                // Catch poll and destructor panics independently, so one cleanup
                // cannot prevent other registered releases from completing.
                self.pending.push(Box::pin(async move {
                    let mut future = Some(future);
                    let panicked = AssertUnwindSafe(future.as_mut().unwrap())
                        .catch_unwind()
                        .await
                        .is_err();
                    let drop_panicked =
                        catch_unwind(AssertUnwindSafe(|| drop(future.take()))).is_err();
                    panicked || drop_panicked
                }));
            }
            match self.pending.poll_next_unpin(cx) {
                Poll::Ready(Some(failed)) => self.failed |= failed,
                Poll::Pending => {
                    if lock(&context.inner.state).cleanup.is_empty() {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => {
                    let mut state = lock(&context.inner.state);
                    if !state.cleanup.is_empty() {
                        continue;
                    }
                    let result = if self.failed {
                        Err(CleanupFailed)
                    } else {
                        Ok(())
                    };
                    state.closed = Some(result);
                    return Poll::Ready(result);
                }
            }
        }
    }
}

/// A call future. Its business future is always polled and dropped in its context.
#[must_use = "poll the call, or retain its context and wait_closed after dropping it"]
pub struct Run<F: Future> {
    context: CallContext,
    future: Option<Pin<Box<F>>>,
    cancellation: Option<CancellationSignal>,
    result: Option<Result<F::Output, Cancelled>>,
    panic: Option<Panic>,
    cleanup: Option<Cleanup>,
    done: bool,
}
// Only boxed futures are pinned; result values are never structurally pinned.
impl<F: Future> Unpin for Run<F> {}
impl<F: Future> Run<F> {
    // Both callers (poll through close, and Drop) already hold this context's
    // scope for the entire business/result destruction and panic handling.
    fn drop_business_in_scope(&mut self) {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(self.future.take()))) {
            if self.panic.is_none() {
                self.panic = Some(panic);
            }
        }
    }
    fn close(&mut self) {
        self.drop_business_in_scope();
        self.cleanup = self.context.begin_close();
    }
}
impl<F: Future> Future for Run<F> {
    type Output = Result<F::Output, Cancelled>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(!this.done, "a completed call cannot be polled again");
        let _scope = Scope::enter(&this.context);
        if this.future.is_some() {
            if this.context.inner.token.is_cancelled() {
                this.result = Some(Err(Cancelled));
            } else {
                match catch_unwind(AssertUnwindSafe(|| {
                    this.future.as_mut().unwrap().as_mut().poll(cx)
                })) {
                    Ok(Poll::Pending) => {
                        // Ready calls never allocate a cancellation waiter. A
                        // Pending call must register before returning to its
                        // executor, with a recheck to cover cancel during poll.
                        if this.cancellation.is_none() {
                            this.cancellation = this.context.inner.token.signal();
                        }
                        if let Some(signal) = &mut this.cancellation {
                            if Pin::new(signal).poll(cx).is_pending() {
                                return Poll::Pending;
                            }
                        }
                        this.result = Some(Err(Cancelled));
                    }
                    Ok(Poll::Ready(value)) => {
                        // This acquire load linearizes result commitment against
                        // cancel's atomic swap. Later cancellation cannot replace
                        // a result already committed before cleanup.
                        let cancelled = this.context.inner.token.is_cancelled();
                        if cancelled {
                            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
                                this.panic = Some(panic);
                            }
                            this.result = Some(Err(Cancelled));
                        } else {
                            this.result = Some(Ok(value));
                        }
                    }
                    Err(panic) => this.panic = Some(panic),
                }
            }
            this.close();
        }
        let cleanup = match this.cleanup.as_mut() {
            Some(cleanup) => match Pin::new(cleanup).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result,
            },
            None => Ok(()),
        };
        this.done = true;
        if let Some(panic) = this.panic.take() {
            resume_unwind(panic);
        }
        if cleanup.is_err() {
            panic!("a registered call cleanup panicked");
        }
        Poll::Ready(this.result.take().unwrap())
    }
}
impl<F: Future> Drop for Run<F> {
    fn drop(&mut self) {
        // `done` alone is insufficient: cleanup or business Drop can panic
        // after it is set, leaving a result that must still be dropped in scope.
        if self.done && self.future.is_none() && self.result.is_none() && self.panic.is_none() {
            return;
        }
        let _scope = Scope::enter(&self.context);
        if self.future.is_some() {
            self.drop_business_in_scope();
            self.context.begin_close();
        }
        // A pending result can own resources too. Drop it before leaving scope.
        drop(self.result.take());
        // Drop cannot await. wait_closed remains available to the owner. Panics
        // during business drop are propagated unless we are already unwinding.
        if !std::thread::panicking() {
            if let Some(panic) = self.panic.take() {
                resume_unwind(panic);
            }
        }
    }
}
