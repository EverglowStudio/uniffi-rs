use super::{BoxFuture, SendOutsideWasm};
use futures_channel::oneshot;
use futures_util::{
    future::Shared, stream::FuturesUnordered, task::AtomicWaker, FutureExt, StreamExt,
};
use parking_lot::Mutex;
use std::{
    any::Any,
    cell::RefCell,
    fmt,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
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

/// A cancellation request. Cancellation is not proof that a call has closed.
#[derive(Clone)]
pub struct CancellationToken {
    state: Arc<TokenState>,
}
struct TokenState {
    cancelled: AtomicBool,
    notification: Notify,
    #[cfg(not(target_arch = "wasm32"))]
    context: Weak<Inner>,
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
        Self::with_context(Weak::new())
    }
    fn with_context(_context: Weak<Inner>) -> Self {
        Self {
            state: Arc::new(TokenState {
                cancelled: AtomicBool::new(false),
                notification: Notify::new(),
                #[cfg(not(target_arch = "wasm32"))]
                context: _context,
            }),
        }
    }
    /// Request cancellation, idempotently. Wakes all registered waiters.
    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.notification.notify_waiters();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(context) = self.state.context.upgrade() {
            context.cleanup_waker.wake();
        }
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
        // OwnedNotified without making cancellation a structurally pinned field.
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
// Owner cancellation must not retain a finished call or its cleanup captures.
pub(super) struct WeakCallContext(Weak<Inner>);
impl WeakCallContext {
    pub(super) fn cancel(&self) {
        if let Some(inner) = self.0.upgrade() {
            CallContext { inner }.cancel();
        }
    }
}
struct Inner {
    token: OnceLock<CancellationToken>,
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
    pub(super) fn downgrade(&self) -> WeakCallContext {
        WeakCallContext(ContextRef::downgrade(&self.inner))
    }
    pub fn new(token: CancellationToken) -> Self {
        Self::with_token(token.into())
    }
    // Internal scopes have no external token until it is requested. They retain
    // the same cancellation path when exposed through current_context.
    pub(super) fn fresh() -> Self {
        Self::with_token(OnceLock::new())
    }
    fn with_token(token: OnceLock<CancellationToken>) -> Self {
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
        self.token().cancel();
    }
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token().clone()
    }
    fn token(&self) -> &CancellationToken {
        // Merely exposing a token does not wake the business future. Its weak
        // link forwards an actual cancellation to a still-fresh pending Run.
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner
                .token
                .get_or_init(|| CancellationToken::with_context(ContextRef::downgrade(&self.inner)))
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.inner.token.get_or_init(CancellationToken::new)
        }
    }
    fn is_cancelled(&self) -> bool {
        self.inner
            .token
            .get()
            .is_some_and(CancellationToken::is_cancelled)
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
            let mut state = self.inner.state.lock();
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
            future: Some(future),
            cancellation: None,
            business_waker_registered: false,
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
                let mut state = self.inner.state.lock();
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
            let mut state = self.inner.state.lock();
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
    use std::sync::{atomic::AtomicUsize, Barrier, Mutex, MutexGuard};

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn fresh_context_first_pending_cancel_shares_token_and_waits_for_cleanup() {
        let context = CallContext::fresh();
        let start = Arc::new(Barrier::new(9));
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let context = context.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    let token = context.cancellation_token();
                    if index == 0 {
                        token.cancel();
                    }
                    token
                })
            })
            .collect();
        let (started, mut cleaning) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        context
            .defer_cleanup(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
            })
            .unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let mut call = Box::pin(context.run(futures::future::poll_fn(move |_| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                start.wait();
            }
            Poll::<u32>::Pending
        })));
        futures::executor::block_on(futures::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Pin::new(&mut cleaning).poll(cx)
        }))
        .unwrap();
        for worker in workers {
            assert!(worker.join().unwrap().is_cancelled());
        }
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        futures::executor::block_on(async {
            let closed = context.wait_closed();
            futures::pin_mut!(closed);
            assert!(futures::poll!(&mut closed).is_pending());
            assert!(futures::poll!(&mut call).is_pending());
            release.send(()).unwrap();
            assert_eq!(call.await, Err(Cancelled));
            assert_eq!(closed.await, Ok(()));
        });
    }

    #[test]
    fn fresh_context_ready_cancel_drops_value_in_scope_before_ack() {
        struct Value {
            gate: Option<oneshot::Receiver<()>>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Value {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                let gate = self.gate.take().unwrap();
                current_context()
                    .unwrap()
                    .defer_cleanup(async move { gate.await.unwrap() })
                    .unwrap();
            }
        }
        let context = CallContext::fresh();
        let drops = Arc::new(AtomicUsize::new(0));
        let (release, gate) = oneshot::channel();
        let value = Value {
            gate: Some(gate),
            drops: drops.clone(),
        };
        futures::executor::block_on(async {
            let call = context.run(async {
                current_context().unwrap().cancel();
                value
            });
            futures::pin_mut!(call);
            assert!(futures::poll!(&mut call).is_pending());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let closed = context.wait_closed();
            futures::pin_mut!(closed);
            assert!(futures::poll!(&mut closed).is_pending());
            release.send(()).unwrap();
            assert!(matches!(call.await, Err(Cancelled)));
            assert_eq!(closed.await, Ok(()));
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fresh_context_pre_cancel_and_late_cancel_preserve_outcomes() {
        let early = CallContext::fresh();
        early.cancel();
        assert_eq!(
            futures::executor::block_on(
                early.run(async { panic!("pre-cancelled business must not be polled") })
            ),
            Err::<(), _>(Cancelled)
        );
        let late = CallContext::fresh();
        assert_eq!(futures::executor::block_on(late.run(async { 42 })), Ok(42));
        late.cancellation_token().cancel();
        assert_eq!(futures::executor::block_on(late.wait_closed()), Ok(()));
        assert!(late.cancellation_token().is_cancelled());
    }

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
        let mut call = Box::pin(context.run(async { 42 }));
        assert!(call.cancellation.is_none());
        assert!(context.inner.state.lock().notification.is_none());
        assert_eq!(futures::executor::block_on(&mut call), Ok(42));
        assert!(call.cancellation.is_none());
        futures::executor::block_on(async {
            let (one, two) = futures::join!(context.wait_closed(), context.wait_closed());
            assert_eq!((one, two), (Ok(()), Ok(())));
        });
        let state = context.inner.state.lock();
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
            let mut call = Box::pin(context.run(async { panic!("business must not run") }));
            assert!(call.cancellation.is_none());
            if cancelled {
                token.cancel();
                assert_eq!(futures::executor::block_on(&mut call), Err(Cancelled));
                assert!(call.cancellation.is_none());
            }
            drop(call);
            assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
            assert_eq!(Arc::strong_count(&token.state), 2);
            let state = context.inner.state.lock();
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
        assert!(context.inner.state.lock().notification.is_some());
        drop(one);
        assert_eq!(futures::executor::block_on(context.run(async { 7 })), Ok(7));
        assert_eq!(futures::executor::block_on(two), Ok(()));
        assert!(context.inner.state.lock().closing.is_none());
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

    // Cell deliberately keeps this business future Send but not Sync.
    struct InlineProbe {
        expected: CallContext,
        events: Arc<Mutex<Vec<(usize, bool)>>>,
        polls: std::cell::Cell<usize>,
        mode: u8,
        panic_on_drop: bool,
        release: Option<oneshot::Receiver<()>>,
        cleaned: Arc<AtomicUsize>,
        _pin: std::marker::PhantomPinned,
    }
    impl InlineProbe {
        fn record(&self) {
            lock(&self.events).push((
                self as *const Self as usize,
                current_context().is_some_and(|current| {
                    ContextRef::ptr_eq(&current.inner, &self.expected.inner)
                }),
            ));
        }
    }
    impl Future for InlineProbe {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            self.record();
            let polls = self.polls.get() + 1;
            self.polls.set(polls);
            match (self.mode, polls) {
                (0, 2) => Poll::Ready(()),
                (5, 2) => panic!("inline business poll panic"),
                _ => Poll::Pending,
            }
        }
    }
    impl Drop for InlineProbe {
        fn drop(&mut self) {
            self.record();
            let release = self.release.take().unwrap();
            let cleaned = self.cleaned.clone();
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    release.await.unwrap();
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
            if self.panic_on_drop {
                panic!("inline business drop panic");
            }
        }
    }

    #[test]
    fn inline_future_keeps_its_address_and_drops_once_in_its_context() {
        static_assertions::assert_not_impl_any!(InlineProbe: Unpin, Sync);
        static_assertions::assert_not_impl_any!(Run<InlineProbe>: Unpin);
        static_assertions::assert_impl_all!(Run<InlineProbe>: Send);
        // Ready, cancel after Pending, Drop after Pending, unpolled Drop,
        // pre-cancel, and poll panic; each also exercises destructor unwinding.
        for mode in 0..6 {
            for panic_on_drop in [false, true] {
                let context = CallContext::new(CancellationToken::new());
                let events = Arc::new(Mutex::new(Vec::new()));
                let cleaned = Arc::new(AtomicUsize::new(0));
                let (release, released) = oneshot::channel();
                let mut call = Box::pin(context.run(InlineProbe {
                    expected: context.clone(),
                    events: events.clone(),
                    polls: std::cell::Cell::new(0),
                    mode,
                    panic_on_drop,
                    release: Some(released),
                    cleaned: cleaned.clone(),
                    _pin: std::marker::PhantomPinned,
                }));
                let mut cx = Context::from_waker(std::task::Waker::noop());
                if mode == 4 {
                    context.cancel();
                } else if mode != 3 {
                    assert!(call.as_mut().poll(&mut cx).is_pending());
                    assert!(call.as_mut().poll(&mut cx).is_pending());
                    if mode == 1 {
                        context.cancel();
                    }
                }
                if mode == 2 || mode == 3 {
                    assert_eq!(
                        catch_unwind(AssertUnwindSafe(|| drop(call))).is_err(),
                        panic_on_drop
                    );
                    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
                    release.send(()).unwrap();
                } else {
                    assert!(call.as_mut().poll(&mut cx).is_pending());
                    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
                    release.send(()).unwrap();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        futures::executor::block_on(call.as_mut())
                    }));
                    if panic_on_drop || mode == 5 {
                        let panic = result.unwrap_err();
                        let message = panic.downcast_ref::<&str>().unwrap();
                        assert_eq!(
                            *message,
                            if mode == 5 {
                                "inline business poll panic"
                            } else {
                                "inline business drop panic"
                            }
                        );
                    } else {
                        assert_eq!(
                            result.unwrap(),
                            if mode == 0 { Ok(()) } else { Err(Cancelled) }
                        );
                    }
                    drop(call);
                }
                assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
                assert_eq!(cleaned.load(Ordering::SeqCst), 1);
                let events = lock(&events);
                assert_eq!(events.len(), if mode == 3 || mode == 4 { 1 } else { 3 });
                assert!(events.iter().all(|event| *event == (events[0].0, true)));
                assert!(current_context().is_none());
            }
        }
    }

    #[test]
    fn unpin_business_with_non_unpin_output_keeps_run_unpin() {
        use futures::future::{ready, Ready};
        use std::marker::PhantomPinned;
        static_assertions::assert_impl_all!(Run<Ready<PhantomPinned>>: Unpin);
        let context = CallContext::new(CancellationToken::new());
        let mut call = context.run(ready(PhantomPinned));
        let output = futures::executor::block_on(&mut call).unwrap();
        let _ = output;
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod lazy_pending_cancel_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn fresh_pending_external_first_cancel_wakes_and_waits_for_real_cleanup() {
        for direct_cancel in [false, true] {
            let context = CallContext::fresh();
            let (started, mut cleaning) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            context
                .defer_cleanup(async move {
                    started.send(()).unwrap();
                    gate.await.unwrap();
                })
                .unwrap();
            let polls = AtomicUsize::new(0);
            let mut call = Box::pin(context.run(futures::future::poll_fn(|_| {
                polls.fetch_add(1, Ordering::SeqCst);
                Poll::<()>::Pending
            })));
            let count = Arc::new(WakeCount::default());
            let waker = Waker::from(count.clone());
            assert!(call
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            assert_eq!(count.0.load(Ordering::SeqCst), 0);
            let external = context.clone();
            std::thread::spawn(move || {
                if direct_cancel {
                    external.cancel();
                } else {
                    external.cancellation_token().cancel();
                }
            })
            .join()
            .unwrap();
            assert_eq!(count.0.load(Ordering::SeqCst), 1);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(call.as_mut().poll(&mut cx).is_pending());
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert_eq!(cleaning.try_recv(), Ok(Some(())));
            let mut closed = Box::pin(context.wait_closed());
            assert!(closed.as_mut().poll(&mut cx).is_pending());
            release.send(()).unwrap();
            assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Err(Cancelled)));
            assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn first_token_exposure_is_quiet_then_cancel_uses_the_repolled_signal() {
        let context = CallContext::fresh();
        let mut call = Box::pin(context.run(futures::future::pending::<()>()));
        let first = Arc::new(WakeCount::default());
        let first_waker = Waker::from(first.clone());
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&first_waker))
            .is_pending());
        let external = context.clone();
        let token = std::thread::spawn(move || external.cancellation_token())
            .join()
            .unwrap();
        assert!(!token.is_cancelled());
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        let latest = Arc::new(WakeCount::default());
        let latest_waker = Waker::from(latest.clone());
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&latest_waker))
            .is_pending());
        drop(context.cancellation_token());
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 0);
        std::thread::spawn(move || token.cancel()).join().unwrap();
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(Cancelled))
        );
    }

    #[test]
    fn external_token_publication_before_business_pending_preserves_cancel() {
        for cancel_before_pending in [false, true] {
            let context = CallContext::fresh();
            let external = context.clone();
            let (entered, observed) = std::sync::mpsc::sync_channel(0);
            let (published, publication) = std::sync::mpsc::sync_channel(0);
            let worker = std::thread::spawn(move || {
                observed.recv().unwrap();
                let token = external.cancellation_token();
                if cancel_before_pending {
                    token.cancel();
                }
                published.send(()).unwrap();
                token
            });
            let mut first_poll = true;
            let mut call = Box::pin(context.run(futures::future::poll_fn(|_| {
                if first_poll {
                    first_poll = false;
                    entered.send(()).unwrap();
                    publication.recv().unwrap();
                }
                Poll::<()>::Pending
            })));
            let count = Arc::new(WakeCount::default());
            let waker = Waker::from(count.clone());
            let result = call.as_mut().poll(&mut Context::from_waker(&waker));
            let token = worker.join().unwrap();
            if cancel_before_pending {
                assert_eq!(result, Poll::Ready(Err(Cancelled)));
            } else {
                assert!(result.is_pending());
                assert_eq!(count.0.load(Ordering::SeqCst), 0);
                std::thread::spawn(move || token.cancel()).join().unwrap();
                assert_eq!(count.0.load(Ordering::SeqCst), 1);
                assert_eq!(
                    call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(Cancelled))
                );
            }
        }
    }

    #[test]
    fn fresh_business_waker_is_released_on_ready_cancel_drop_and_panic() {
        // Keep the context alive past each terminal path and observe only the
        // executor's Wake allocation, not the Run's private registration flag.
        for mode in 0..4 {
            let context = CallContext::fresh();
            let finish = AtomicBool::new(false);
            let mut call = Box::pin(context.run(futures::future::poll_fn(|_| {
                if !finish.load(Ordering::SeqCst) {
                    Poll::Pending
                } else if mode == 3 {
                    panic!("fresh business poll panic");
                } else {
                    Poll::Ready(42)
                }
            })));
            let count = Arc::new(WakeCount::default());
            let weak = Arc::downgrade(&count);
            let waker = Waker::from(count);
            assert!(call
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            drop(waker);
            assert!(weak.upgrade().is_some());
            let mut cx = Context::from_waker(Waker::noop());
            match mode {
                0 => {
                    finish.store(true, Ordering::SeqCst);
                    assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));
                    assert!(weak.upgrade().is_none());
                    drop(call);
                }
                1 => {
                    context.cancel();
                    assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Err(Cancelled)));
                    assert!(weak.upgrade().is_none());
                    drop(call);
                }
                2 => drop(call),
                _ => {
                    finish.store(true, Ordering::SeqCst);
                    assert!(
                        catch_unwind(AssertUnwindSafe(|| { call.as_mut().poll(&mut cx) })).is_err()
                    );
                    assert!(weak.upgrade().is_none());
                    drop(call);
                }
            }
            assert!(weak.upgrade().is_none());
            assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn fresh_pending_replaces_business_waker_and_ready_never_retains_one() {
        let context = CallContext::fresh();
        let mut call = Box::pin(context.run(futures::future::pending::<()>()));
        let old = Arc::new(WakeCount::default());
        let weak_old = Arc::downgrade(&old);
        let old_waker = Waker::from(old);
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&old_waker))
            .is_pending());
        drop(old_waker);
        assert!(weak_old.upgrade().is_some());
        let latest = Arc::new(WakeCount::default());
        let latest_waker = Waker::from(latest.clone());
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&latest_waker))
            .is_pending());
        assert!(weak_old.upgrade().is_none());
        drop(context.cancellation_token());
        assert_eq!(latest.0.load(Ordering::SeqCst), 0);
        context.cancel();
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
        drop(call);

        let ready_context = CallContext::fresh();
        let mut ready = Box::pin(ready_context.run(futures::future::ready(42)));
        let count = Arc::new(WakeCount::default());
        let weak = Arc::downgrade(&count);
        let waker = Waker::from(count);
        assert_eq!(
            ready.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(42))
        );
        drop(waker);
        assert!(weak.upgrade().is_none());
        drop(ready);
        assert_eq!(
            futures::executor::block_on(ready_context.wait_closed()),
            Ok(())
        );
    }

    #[test]
    fn dropping_run_after_close_preserves_the_cleanup_registration() {
        let context = CallContext::fresh();
        let (release, gate) = oneshot::channel();
        context
            .defer_cleanup(async move { gate.await.unwrap() })
            .unwrap();
        let finish = AtomicBool::new(false);
        let mut call = Box::pin(context.run(futures::future::poll_fn(|_| {
            if finish.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })));
        let business = Arc::new(WakeCount::default());
        let weak_business = Arc::downgrade(&business);
        let business_waker = Waker::from(business);
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&business_waker))
            .is_pending());
        drop(business_waker);
        assert!(weak_business.upgrade().is_some());
        finish.store(true, Ordering::SeqCst);
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert!(weak_business.upgrade().is_none());
        let cleanup = Arc::new(WakeCount::default());
        let weak_cleanup = Arc::downgrade(&cleanup);
        let cleanup_waker = Waker::from(cleanup.clone());
        let mut cx = Context::from_waker(&cleanup_waker);
        let mut closed = Box::pin(context.wait_closed());
        assert!(closed.as_mut().poll(&mut cx).is_pending());
        drop(call);
        let before = cleanup.0.load(Ordering::SeqCst);
        context.defer_cleanup(async {}).unwrap();
        assert!(cleanup.0.load(Ordering::SeqCst) > before);
        assert!(closed.as_mut().poll(&mut cx).is_pending());
        release.send(()).unwrap();
        assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        drop(closed);
        drop(cleanup_waker);
        drop(cleanup);
        assert!(weak_cleanup.upgrade().is_none());
        assert_eq!(futures::executor::block_on(context.wait_closed()), Ok(()));
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
            let queued = std::mem::take(&mut context.inner.state.lock().cleanup);
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
                    if context.inner.state.lock().cleanup.is_empty() {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => {
                    let mut state = context.inner.state.lock();
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

pin_project_lite::pin_project! {
    /// A call future. Its business future is polled and dropped in place in its context.
    ///
    /// `Run<F>` is `Unpin` when `F` is `Unpin`; its output is not structurally pinned.
    #[must_use = "poll the call, or retain its context and wait_closed after dropping it"]
    #[project = RunProjection]
    pub struct Run<F: Future> {
        context: CallContext,
        #[pin]
        future: Option<F>,
        cancellation: Option<CancellationSignal>,
        business_waker_registered: bool,
        result: Option<Result<F::Output, Cancelled>>,
        panic: Option<Panic>,
        cleanup: Option<Cleanup>,
        done: bool,
    }

    impl<F: Future> PinnedDrop for Run<F> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            // `done` alone is insufficient: cleanup or business Drop can panic
            // after it is set, leaving a result that must still be dropped in scope.
            if *this.done && this.future.is_none() && this.result.is_none() && this.panic.is_none() {
                return;
            }
            let _scope = Scope::enter(this.context);
            if this.future.is_some() {
                this.drop_business_in_scope();
                this.context.begin_close();
            }
            // A pending result can own resources too. Drop it before leaving scope.
            drop(this.result.take());
            // Drop cannot await. wait_closed remains available to the owner. Panics
            // during business drop are propagated unless we are already unwinding.
            if !std::thread::panicking() {
                if let Some(panic) = this.panic.take() {
                    resume_unwind(panic);
                }
            }
        }
    }
}

impl<F: Future> RunProjection<'_, F> {
    // Both callers (poll through close, and Drop) already hold this context's
    // scope for the entire business/result destruction and panic handling.
    fn drop_business_in_scope(&mut self) {
        // Business and CleanupDriver never register concurrently. Release our
        // executor before begin_close hands this slot to the cleanup driver.
        if std::mem::take(self.business_waker_registered) {
            self.context.inner.cleanup_waker.take();
        }
        // Assignment installs None even when the old destructor unwinds, so
        // catching that panic cannot leave a future to poll or destroy again.
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.future.set(None))) {
            if self.panic.is_none() {
                *self.panic = Some(panic);
            }
        }
    }
    fn close(&mut self) {
        self.drop_business_in_scope();
        *self.cleanup = self.context.begin_close();
    }
}
impl<F: Future> Future for Run<F> {
    type Output = Result<F::Output, Cancelled>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        assert!(!*this.done, "a completed call cannot be polled again");
        let _scope = Scope::enter(this.context);
        if this.future.is_some() {
            if this.context.is_cancelled() {
                *this.result = Some(Err(Cancelled));
            } else {
                match catch_unwind(AssertUnwindSafe(|| {
                    this.future.as_mut().as_pin_mut().unwrap().poll(cx)
                })) {
                    Ok(Poll::Pending) => {
                        // Native can link a Send/Sync context weakly. WASM
                        // contexts are local Rc values; keep tokens transferable
                        // there by retaining their original Notify signal path.
                        #[cfg(not(target_arch = "wasm32"))]
                        if this.context.inner.token.get().is_none() {
                            *this.business_waker_registered = true;
                            this.context.inner.cleanup_waker.register(cx.waker());
                            // Publication before registration is covered by this
                            // recheck. Later cancellation reaches this slot via
                            // the token's weak context link.
                            if this.context.inner.token.get().is_none() {
                                return Poll::Pending;
                            }
                        }
                        // A materialized token uses the original signal; retire
                        // the fresh registration before installing its waiter.
                        if std::mem::take(this.business_waker_registered) {
                            this.context.inner.cleanup_waker.take();
                        }
                        // Ready calls never allocate a cancellation waiter. A
                        // Pending call must register before returning to its
                        // executor, with a recheck to cover cancel during poll.
                        if this.cancellation.is_none() {
                            *this.cancellation = this.context.token().signal();
                        }
                        if let Some(signal) = this.cancellation.as_mut() {
                            if Pin::new(signal).poll(cx).is_pending() {
                                return Poll::Pending;
                            }
                        }
                        *this.result = Some(Err(Cancelled));
                    }
                    Ok(Poll::Ready(value)) => {
                        // This acquire load linearizes result commitment against
                        // cancel's atomic swap. Later cancellation cannot replace
                        // a result already committed before cleanup.
                        let cancelled = this.context.is_cancelled();
                        if cancelled {
                            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
                                *this.panic = Some(panic);
                            }
                            *this.result = Some(Err(Cancelled));
                        } else {
                            *this.result = Some(Ok(value));
                        }
                    }
                    Err(panic) => *this.panic = Some(panic),
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
        *this.done = true;
        if let Some(panic) = this.panic.take() {
            resume_unwind(panic);
        }
        if cleanup.is_err() {
            panic!("a registered call cleanup panicked");
        }
        Poll::Ready(this.result.take().unwrap())
    }
}
