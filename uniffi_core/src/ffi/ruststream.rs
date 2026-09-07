/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(not(all(feature = "tokio", not(target_arch = "wasm32"))))]
use crate::rust_future_new;
use crate::{
    check_remaining, FutureLowerReturn, Handle, Lift, LiftArgsError, Lower, LowerReturn,
    RustBuffer, RustCallError,
};
use anyhow::{anyhow, bail};
use bytes::{Buf, BufMut};
use futures_core::Stream;
use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
};

/// Rust stream type supported by the strict native output-stream ABI.
#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub type UniFfiStream<T, E> = Pin<Box<dyn Stream<Item = Result<T, E>> + Send + 'static>>;

/// Single-threaded wasm stream type reserved for the JavaScript wasm backend.
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub type UniFfiStream<T, E> = Pin<Box<dyn Stream<Item = Result<T, E>> + 'static>>;

/// Apply the same runtime scope as native C ABI stream next/close drivers.
/// Backends with their own value converters pass the whole CallContext Run.
#[doc(hidden)]
pub fn rust_stream_runtime<F: Future>(future: F) -> impl Future<Output = F::Output> {
    #[cfg(all(feature = "tokio", not(target_arch = "wasm32")))]
    {
        async_compat::Compat::new(future)
    }
    #[cfg(not(all(feature = "tokio", not(target_arch = "wasm32"))))]
    {
        future
    }
}

/// One native output-stream transition.
///
/// This is intentionally a three-variant enum rather than a record with
/// optional fields: a stream pull is exactly one of an item, normal
/// completion, or a typed business error.  In particular, `Item(None)` for
/// `UniFfiStream<Option<T>, E>` is distinct from `Done`.
#[doc(hidden)]
#[derive(Debug, PartialEq, Eq)]
pub enum UniFfiStreamStep<T, E> {
    Item(T),
    Done,
    Error(E),
}

/// Framework cancellation of an accepted next, separate from business E/EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamCancelled;
impl std::fmt::Display for StreamCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stream next cancelled")
    }
}
impl std::error::Error for StreamCancelled {}

// These tags are part of the native output-stream RustBuffer ABI. Keep them
// explicit and independent from every item or error encoding so bindings can
// reject malformed/unknown steps before lifting a payload.
#[doc(hidden)]
pub const UNIFFI_STREAM_STEP_ITEM_TAG: u8 = 1;
#[doc(hidden)]
pub const UNIFFI_STREAM_STEP_DONE_TAG: u8 = 2;
#[doc(hidden)]
pub const UNIFFI_STREAM_STEP_ERROR_TAG: u8 = 3;

unsafe impl<UT, T, E> Lower<UT> for UniFfiStreamStep<T, E>
where
    T: Lower<UT>,
    E: Lower<UT>,
{
    type FfiType = RustBuffer;

    fn lower(obj: Self) -> RustBuffer {
        Self::lower_into_rust_buffer(obj)
    }

    fn write(obj: Self, buf: &mut Vec<u8>) {
        match obj {
            Self::Item(item) => {
                buf.put_u8(UNIFFI_STREAM_STEP_ITEM_TAG);
                T::write(item, buf);
            }
            Self::Done => buf.put_u8(UNIFFI_STREAM_STEP_DONE_TAG),
            Self::Error(error) => {
                buf.put_u8(UNIFFI_STREAM_STEP_ERROR_TAG);
                E::write(error, buf);
            }
        }
    }
}

unsafe impl<UT, T, E> Lift<UT> for UniFfiStreamStep<T, E>
where
    T: Lift<UT>,
    E: Lift<UT>,
{
    type FfiType = RustBuffer;

    fn try_lift(buf: RustBuffer) -> crate::Result<Self> {
        Self::try_lift_from_rust_buffer(buf)
    }

    fn try_read(buf: &mut &[u8]) -> crate::Result<Self> {
        check_remaining(buf, 1)?;
        match buf.get_u8() {
            UNIFFI_STREAM_STEP_ITEM_TAG => Ok(Self::Item(T::try_read(buf)?)),
            UNIFFI_STREAM_STEP_DONE_TAG => Ok(Self::Done),
            UNIFFI_STREAM_STEP_ERROR_TAG => Ok(Self::Error(E::try_read(buf)?)),
            tag => bail!("unexpected UniFFI output stream step tag: {tag}"),
        }
    }
}

unsafe impl<UT, T, E> LowerReturn<UT> for UniFfiStreamStep<T, E>
where
    T: Lower<UT>,
    E: Lower<UT>,
{
    type ReturnType = RustBuffer;

    fn lower_return(value: Self) -> Result<Self::ReturnType, RustCallError> {
        Ok(<Self as Lower<UT>>::lower(value))
    }
    fn handle_failed_lift(error: LiftArgsError) -> Result<Self::ReturnType, RustCallError> {
        if error.error.is::<StreamCancelled>() {
            Err(RustCallError::Cancelled)
        } else {
            Err(error.to_internal_error())
        }
    }
}

/// Marker trait for values that can be stored in a UniFFI Rust stream registry.
///
/// Native targets keep the same `Send + 'static` guarantee as Rust futures. The
/// wasm single-threaded JavaScript backend can opt into local, non-`Send` stream
/// state because JS drives Rust on one thread.
#[doc(hidden)]
#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub trait UniffiCompatibleStreamValue: Send + 'static {}

#[doc(hidden)]
#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
impl<T> UniffiCompatibleStreamValue for T where T: Send + 'static {}

#[doc(hidden)]
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub trait UniffiCompatibleStreamValue: 'static {}

#[doc(hidden)]
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
impl<T> UniffiCompatibleStreamValue for T where T: 'static {}

/// Per-stream-function registry generated by scaffolding.
/// WASM stores values in instance-local TLS; its static control contains no values.
#[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
pub type RustStreamRegistry<T, E> = Lazy<Mutex<HashMap<u64, Arc<RustStream<T, E>>>>>;

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub type RustStreamRegistry<T, E> = Lazy<LocalStreamRegistry<T, E>>;

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
pub struct LocalStreamRegistry<T, E> {
    id: u64,
    marker: std::marker::PhantomData<fn() -> (T, E)>,
}
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
impl<T, E> Default for LocalStreamRegistry<T, E> {
    fn default() -> Self {
        Self {
            id: NEXT_STREAM_HANDLE.fetch_add(2, Ordering::Relaxed),
            marker: std::marker::PhantomData,
        }
    }
}
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
static_assertions::assert_impl_all!(RustStreamRegistry<std::rc::Rc<u32>, std::rc::Rc<()>>: Sync);
#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
static_assertions::assert_not_impl_any!(RustStream<std::rc::Rc<u32>, std::rc::Rc<()>>: Send, Sync);

#[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
thread_local! {
    static LOCAL_REGISTRIES: std::cell::RefCell<HashMap<u64, Box<dyn std::any::Any>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn with_registry<T: 'static, E: 'static, R>(
    registry: &'static RustStreamRegistry<T, E>,
    f: impl FnOnce(&mut HashMap<u64, Arc<RustStream<T, E>>>) -> R,
) -> R {
    #[cfg(not(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded")))]
    {
        f(&mut registry.lock().unwrap())
    }
    #[cfg(all(target_arch = "wasm32", feature = "wasm-unstable-single-threaded"))]
    {
        LOCAL_REGISTRIES.with(|slot| {
            let mut maps = slot.borrow_mut();
            let map = maps
                .entry(registry.id)
                .or_insert_with(|| Box::new(HashMap::<u64, Arc<RustStream<T, E>>>::new()));
            f(map.downcast_mut().expect("typed stream registry"))
        })
    }
}

#[cfg(feature = "async-runtime")]
use crate::async_runtime::{
    current_context, BoxFuture, CallContext, CancellationToken, CleanupFailed, Run, TaskError,
};

// Run establishes the subscription context on every business poll and destructor.
// It is only polled by an actual next request. The slot contains at most one item.
#[cfg(feature = "async-runtime")]
struct StreamDriver<T, E> {
    stream: UniFfiStream<T, E>,
    slot: Arc<Mutex<Poll<Option<Result<T, E>>>>>,
}
#[cfg(feature = "async-runtime")]
impl<T, E> Future for StreamDriver<T, E> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let item = self.stream.as_mut().poll_next(cx);
        *self.slot.lock().unwrap() = item;
        Poll::Pending
    }
}
#[cfg(feature = "async-runtime")]
thread_local! {
    static ACTIVE_STREAM: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static STREAM_POLL_FAILURE: std::cell::Cell<Option<TaskError>> = const { std::cell::Cell::new(None) };
}
// A Stream<Item = Result<T,E>> cannot manufacture a business E for an owner
// failure. The scoped driver carries that framework terminal out of band.
#[cfg(feature = "async-runtime")]
pub(crate) fn local_stream_stopped(error: TaskError) {
    if ACTIVE_STREAM.with(|slot| slot.get().is_some()) {
        STREAM_POLL_FAILURE.with(|slot| slot.set(Some(error)));
    }
}
#[cfg(feature = "async-runtime")]
struct StreamScope(Option<u64>, Option<TaskError>);
#[cfg(feature = "async-runtime")]
impl StreamScope {
    fn enter(key: u64) -> Self {
        Self(
            ACTIVE_STREAM.with(|slot| slot.replace(Some(key))),
            STREAM_POLL_FAILURE.with(|slot| slot.replace(None)),
        )
    }
}
#[cfg(feature = "async-runtime")]
impl Drop for StreamScope {
    fn drop(&mut self) {
        ACTIVE_STREAM.with(|slot| slot.set(self.0));
        STREAM_POLL_FAILURE.with(|slot| slot.set(self.1));
    }
}
#[cfg(feature = "async-runtime")]
struct ScopedStream<T, E> {
    key: u64,
    run: Option<Run<StreamDriver<T, E>>>,
    slot: Arc<Mutex<Poll<Option<Result<T, E>>>>>,
    failure: Arc<Mutex<Option<TaskError>>>,
}
#[cfg(feature = "async-runtime")]
impl<T, E> Stream for ScopedStream<T, E> {
    type Item = Result<T, E>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let _scope = StreamScope::enter(self.key);
        let _ = Pin::new(self.run.as_mut().unwrap()).poll(cx);
        if let Some(error) = STREAM_POLL_FAILURE.with(|slot| slot.take()) {
            *self.failure.lock().unwrap() = Some(error);
        }
        std::mem::replace(&mut *self.slot.lock().unwrap(), Poll::Pending)
    }
}

#[cfg(feature = "async-runtime")]
impl<T, E> Drop for ScopedStream<T, E> {
    fn drop(&mut self) {
        let _scope = StreamScope::enter(self.key);
        drop(self.run.take());
    }
}

enum RustStreamState<T, E> {
    Open {
        // Taken only during poll, never while a next waiter is suspended.
        stream: Option<UniFfiStream<T, E>>,
        next_pending: bool,
        pending_waker: Option<Waker>,
    },
    Closed,
}

/// Runtime state for one subscription. No manual Send/Sync implementation.
pub struct RustStream<T, E> {
    cancelled: AtomicBool,
    state: Mutex<RustStreamState<T, E>>,
    #[cfg(feature = "async-runtime")]
    context: CallContext,
    #[cfg(feature = "async-runtime")]
    key: u64,
    #[cfg(feature = "async-runtime")]
    failure: Arc<Mutex<Option<TaskError>>>,
}

impl<T, E> RustStream<T, E> {
    fn new(stream: UniFfiStream<T, E>) -> Self
    where
        T: UniffiCompatibleStreamValue,
        E: UniffiCompatibleStreamValue,
    {
        #[cfg(feature = "async-runtime")]
        let context = CallContext::new(CancellationToken::new());
        #[cfg(feature = "async-runtime")]
        let key = NEXT_STREAM_HANDLE.fetch_add(2, Ordering::Relaxed);
        #[cfg(feature = "async-runtime")]
        let failure = Arc::new(Mutex::new(None));
        #[cfg(feature = "async-runtime")]
        let stream: UniFfiStream<T, E> = {
            let slot = Arc::new(Mutex::new(Poll::Pending));
            Box::pin(ScopedStream {
                key,
                run: Some(context.run(StreamDriver {
                    stream,
                    slot: slot.clone(),
                })),
                slot,
                failure: failure.clone(),
            })
        };
        Self {
            cancelled: AtomicBool::new(false),
            state: Mutex::new(RustStreamState::Open {
                stream: Some(stream),
                next_pending: false,
                pending_waker: None,
            }),
            #[cfg(feature = "async-runtime")]
            context,
            #[cfg(feature = "async-runtime")]
            key,
            #[cfg(feature = "async-runtime")]
            failure,
        }
    }

    fn begin_next(self: Arc<Self>) -> RustStreamNext<T, E> {
        let (active, start_error) = {
            let mut state = self.state.lock().unwrap();
            match &mut *state {
                RustStreamState::Open {
                    next_pending: true, ..
                } => (false, Some("concurrent stream next() is not supported")),
                RustStreamState::Open { next_pending, .. } => {
                    *next_pending = true;
                    (true, None)
                }
                RustStreamState::Closed => (false, None),
            }
        };
        RustStreamNext {
            owner: Some(self),
            active,
            start_error,
        }
    }

    fn terminal_step(&self) -> Result<UniFfiStreamStep<T, E>, LiftArgsError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(LiftArgsError {
                arg_name: "stream",
                error: StreamCancelled.into(),
            })
        } else {
            Ok(UniFfiStreamStep::Done)
        }
    }

    fn cancel(&self) {
        let previous = {
            let mut state = self.state.lock().unwrap();
            if matches!(*state, RustStreamState::Open { .. }) {
                self.cancelled.store(true, Ordering::Release);
            }
            std::mem::replace(&mut *state, RustStreamState::Closed)
        };
        if let RustStreamState::Open {
            stream,
            pending_waker,
            ..
        } = previous
        {
            // Drop and RawWaker callbacks must remain outside the state lock.
            // An in-progress poll returns/drops its stream before cleanup can finish.
            if let Some(waker) = pending_waker {
                waker.wake();
            }
            drop(stream);
        }
    }
}

pub struct RustStreamNext<T, E> {
    owner: Option<Arc<RustStream<T, E>>>,
    active: bool,
    start_error: Option<&'static str>,
}
impl<T, E> Unpin for RustStreamNext<T, E> {}

// Restores the stream even when business poll unwinds. A concurrent close instead
// drops it here, after leaving the state lock, in its subscription Run scope.
struct PollLease<T, E> {
    owner: Arc<RustStream<T, E>>,
    stream: Option<UniFfiStream<T, E>>,
    release_next: bool,
}
impl<T, E> Drop for PollLease<T, E> {
    fn drop(&mut self) {
        {
            let mut state = self.owner.state.lock().unwrap();
            if let RustStreamState::Open {
                stream,
                next_pending,
                ..
            } = &mut *state
            {
                *stream = self.stream.take();
                // Reopen admission only when the stream is back in its slot.
                // Pending polls and unwinding retain the current next's claim.
                if self.release_next {
                    *next_pending = false;
                }
            }
        }
        drop(self.stream.take());
    }
}

impl<T, E> Future for RustStreamNext<T, E> {
    type Output = Result<UniFfiStreamStep<T, E>, LiftArgsError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(message) = self.start_error.take() {
            self.owner = None;
            return Poll::Ready(Err(LiftArgsError {
                arg_name: "stream",
                error: anyhow!(message),
            }));
        }
        let Some(owner) = self.owner.as_ref().cloned() else {
            return Poll::Ready(Ok(UniFfiStreamStep::Done));
        };
        let candidate = cx.waker().clone();
        let (stream, stale) = {
            let mut state = owner.state.lock().unwrap();
            match &mut *state {
                RustStreamState::Open {
                    stream,
                    pending_waker,
                    ..
                } => (stream.take(), pending_waker.replace(candidate)),
                RustStreamState::Closed => (None, Some(candidate)),
            }
        };
        drop(stale);
        let Some(stream) = stream else {
            self.active = false;
            self.owner = None;
            return Poll::Ready(owner.terminal_step());
        };
        let mut lease = PollLease {
            owner: owner.clone(),
            stream: Some(stream),
            release_next: false,
        };
        // A custom waker destructor may have reentered cancel while replacing it.
        let closed = matches!(*owner.state.lock().unwrap(), RustStreamState::Closed);
        let polled = if closed {
            Poll::Ready(None)
        } else {
            lease.stream.as_mut().unwrap().as_mut().poll_next(cx)
        };
        let terminal = matches!(polled, Poll::Ready(None | Some(Err(_))));
        let mut stale = None;
        let closed = {
            let mut state = owner.state.lock().unwrap();
            match &mut *state {
                RustStreamState::Closed => true,
                RustStreamState::Open { pending_waker, .. } => {
                    if polled.is_ready() {
                        stale = pending_waker.take();
                    }
                    if terminal {
                        *state = RustStreamState::Closed;
                    }
                    false
                }
            }
        };
        drop(stale);
        let polled = if closed {
            // A cancelled, not-yet-committed item can itself own local proxies
            // or input streams. Drop it before ending the subscription Run and
            // include its asynchronous release in that subscription's cleanup.
            #[cfg(feature = "async-runtime")]
            if let Poll::Ready(Some(value)) = polled {
                let delivery = CallContext::new(CancellationToken::new());
                let dropped = delivery.run(async move {
                    drop(value);
                });
                owner
                    .context
                    .defer_cleanup(async move {
                        delivery
                            .wait_closed()
                            .await
                            .expect("discarded stream item cleanup failed");
                    })
                    .expect("live subscription accepts item cleanup");
                drop(dropped);
            }
            #[cfg(not(feature = "async-runtime"))]
            drop(polled);
            Poll::Ready(None)
        } else {
            polled
        };
        lease.release_next = polled.is_ready();
        drop(lease);
        if closed || polled.is_ready() {
            self.active = false;
            self.owner = None;
        }
        if closed {
            return Poll::Ready(owner.terminal_step());
        }
        #[cfg(feature = "async-runtime")]
        if terminal {
            if let Some(error) = *owner.failure.lock().unwrap() {
                let error = match error {
                    TaskError::Cancelled => StreamCancelled.into(),
                    error => error.into(),
                };
                return Poll::Ready(Err(LiftArgsError {
                    arg_name: "stream",
                    error,
                }));
            }
        }
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(value))) => Poll::Ready(Ok(UniFfiStreamStep::Item(value))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Ok(UniFfiStreamStep::Error(error))),
            Poll::Ready(None) => Poll::Ready(Ok(UniFfiStreamStep::Done)),
        }
    }
}
impl<T, E> Drop for RustStreamNext<T, E> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut stale = None;
        if let Some(owner) = &self.owner {
            let mut state = owner.state.lock().unwrap();
            if let RustStreamState::Open {
                next_pending,
                pending_waker,
                ..
            } = &mut *state
            {
                *next_pending = false;
                stale = pending_waker.take();
            }
        }
        drop(stale);
    }
}

static NEXT_STREAM_HANDLE: AtomicU64 = AtomicU64::new(2);
fn next_stream_handle() -> Handle {
    Handle::from_raw_unchecked(NEXT_STREAM_HANDLE.fetch_add(2, Ordering::Relaxed))
}

pub fn rust_stream_new<T, E>(
    registry: &'static RustStreamRegistry<T, E>,
    stream: UniFfiStream<T, E>,
) -> Handle
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    let handle = next_stream_handle();
    let stream = Arc::new(RustStream::new(stream));
    with_registry(registry, |map| map.insert(handle.as_raw(), stream));
    handle
}

pub fn rust_stream_next<T, E, UT: 'static>(
    registry: &'static RustStreamRegistry<T, E>,
    handle: Handle,
    tag: UT,
) -> Handle
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
    UniFfiStreamStep<T, E>: FutureLowerReturn<UT> + 'static,
{
    let future = registry_next(registry, handle);
    #[cfg(all(feature = "tokio", not(target_arch = "wasm32")))]
    return crate::rust_future_new_tokio(future, tag);
    #[cfg(not(all(feature = "tokio", not(target_arch = "wasm32"))))]
    rust_future_new(future, tag)
}

/// Create the native close-acknowledgement future using the same runtime as
/// stream next. Cancellation of one waiter does not cancel subscription cleanup.
#[doc(hidden)]
#[cfg(feature = "async-runtime")]
pub fn rust_stream_close<T, E, UT: 'static>(
    registry: &'static RustStreamRegistry<T, E>,
    handle: Handle,
    tag: UT,
) -> Handle
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    let future = async move {
        Ok(crate::InternalCallResult(
            rust_stream_close_async(registry, handle)
                .await
                .map_err(|_| RustCallError::InternalError("stream cleanup failed".into())),
        ))
    };
    #[cfg(all(feature = "tokio", not(target_arch = "wasm32")))]
    return crate::rust_future_new_tokio(future, tag);
    #[cfg(not(all(feature = "tokio", not(target_arch = "wasm32"))))]
    rust_future_new(future, tag)
}

pub async fn rust_stream_next_async<T, E>(
    registry: &'static RustStreamRegistry<T, E>,
    handle: Handle,
) -> Result<UniFfiStreamStep<T, E>, LiftArgsError>
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    registry_next(registry, handle).await
}

fn registry_next<T: UniffiCompatibleStreamValue, E: UniffiCompatibleStreamValue>(
    registry: &'static RustStreamRegistry<T, E>,
    handle: Handle,
) -> impl Future<Output = Result<UniFfiStreamStep<T, E>, LiftArgsError>> {
    let stream = with_registry(registry, |map| map.get(&handle.as_raw()).cloned());
    let inner = stream.map(RustStream::begin_next);
    async move {
        let Some(inner) = inner else {
            return Ok(UniFfiStreamStep::Done);
        };
        #[cfg(feature = "async-runtime")]
        let output = {
            use futures_util::FutureExt;
            match std::panic::AssertUnwindSafe(inner).catch_unwind().await {
                Ok(output) => output,
                Err(panic) => {
                    // A terminal destructor may panic after registering cleanup.
                    // Preserve that panic, but first finish subscription release.
                    let _ = rust_stream_close_async(registry, handle).await;
                    std::panic::resume_unwind(panic);
                }
            }
        };
        #[cfg(not(feature = "async-runtime"))]
        let output = inner.await;
        if matches!(
            &output,
            Ok(UniFfiStreamStep::Done | UniFfiStreamStep::Error(_))
        ) || output.as_ref().is_err_and(|error| {
            error.error.is::<StreamCancelled>() || {
                #[cfg(feature = "async-runtime")]
                {
                    error.error.is::<TaskError>()
                }
                #[cfg(not(feature = "async-runtime"))]
                {
                    false
                }
            }
        }) {
            #[cfg(feature = "async-runtime")]
            rust_stream_close_async(registry, handle)
                .await
                .map_err(|error| LiftArgsError {
                    arg_name: "stream",
                    error: error.into(),
                })?;
            #[cfg(not(feature = "async-runtime"))]
            drop(with_registry(registry, |map| map.remove(&handle.as_raw())));
        }
        output
    }
}

/// Request subscription closure. Use `rust_stream_close_async` for confirmation.
pub fn rust_stream_cancel<T, E>(registry: &'static RustStreamRegistry<T, E>, handle: Handle)
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    // Cancellation can destroy an unpolled stream immediately. Keep both that
    // destruction and the close waiter's handoff in the stream runtime scope.
    let mut cancel = std::pin::pin!(rust_stream_runtime(async move {
        #[cfg(feature = "async-runtime")]
        {
            let stream = with_registry(registry, |map| map.get(&handle.as_raw()).cloned());
            if let Some(stream) = stream {
                if ACTIVE_STREAM.with(|slot| slot.get() == Some(stream.key)) {
                    stream.cancel();
                    return;
                }
            }
            // Settle synchronous destruction immediately; hand off remaining work.
            let mut close = Box::pin(rust_stream_close_async(registry, handle));
            let waker = Waker::noop();
            let _ = close.as_mut().poll(&mut Context::from_waker(waker));
        }
        #[cfg(not(feature = "async-runtime"))]
        {
            let stream = with_registry(registry, |map| map.remove(&handle.as_raw()));
            if let Some(stream) = stream {
                stream.cancel();
            }
        }
    }));
    let _ = cancel
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
}

#[cfg(feature = "async-runtime")]
struct CloseOnDrop {
    future: Option<BoxFuture<'static, ()>>,
    key: u64,
}
#[cfg(feature = "async-runtime")]
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let Some(done) = self.future.take() else {
            return;
        };
        if ACTIVE_STREAM.with(|slot| slot.get() == Some(self.key)) {
            return;
        }
        if let Some(parent) = current_context() {
            parent
                .defer_cleanup(done)
                .expect("current call accepts stream cleanup");
        } else {
            // No private runtime: outside an executor, a later explicit close
            // resumes the context retained in the registry.
            #[cfg(not(target_arch = "wasm32"))]
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                drop(runtime.spawn(done));
            }
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(done);
        }
    }
}

#[cfg(feature = "async-runtime")]
async fn wait_stream_closed(context: CallContext, key: u64) -> Result<(), CleanupFailed> {
    let mut wait = Box::pin(context.wait_closed());
    futures_util::future::poll_fn(|cx| {
        let _scope = StreamScope::enter(key);
        wait.as_mut().poll(cx)
    })
    .await
}

/// Close and wait until the Stream destructor and registered cleanup finish.
/// Concurrent/repeated close is idempotent. Dropping a started waiter hands its
/// confirmation to the current CallContext (or the current executor).
#[cfg(feature = "async-runtime")]
pub async fn rust_stream_close_async<T, E>(
    registry: &'static RustStreamRegistry<T, E>,
    handle: Handle,
) -> Result<(), CleanupFailed>
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    let stream = with_registry(registry, |map| map.get(&handle.as_raw()).cloned());
    let Some(stream) = stream else {
        return Ok(());
    };
    let context = stream.context.clone();
    let dropped_context = context.clone();
    let raw = handle.as_raw();
    let key = stream.key;
    let mut guard = CloseOnDrop {
        key,
        future: Some(Box::pin(async move {
            let result = wait_stream_closed(dropped_context, key).await;
            drop(with_registry(registry, |map| map.remove(&raw)));
            result.expect("stream cleanup failed");
        })),
    };
    stream.cancel();
    let result = wait_stream_closed(context, key).await;
    drop(with_registry(registry, |map| map.remove(&raw)));
    guard.future = None;
    result
}

// An error-free stream uses the uninhabited internal error type. TYPE_UNIT is
// solely its metadata marker; no error payload can be constructed or lifted.
mod infallible {
    use crate::{Lift, Lower, LowerReturn, MetadataBuffer, RustBuffer, RustCallError, TypeId};
    use std::convert::Infallible;
    // SAFETY: Infallible has no inhabitants; lowering can never produce a payload.
    unsafe impl<UT> Lower<UT> for Infallible {
        type FfiType = RustBuffer;
        fn lower(value: Self) -> RustBuffer {
            match value {}
        }
        fn write(value: Self, _: &mut Vec<u8>) {
            match value {}
        }
    }
    // SAFETY: lifting always rejects, consuming owned buffers without fabricating a value.
    unsafe impl<UT> Lift<UT> for Infallible {
        type FfiType = RustBuffer;
        fn try_lift(value: RustBuffer) -> crate::Result<Self> {
            drop(value.destroy_into_vec());
            anyhow::bail!("an infallible stream cannot return a business error")
        }
        fn try_read(_: &mut &[u8]) -> crate::Result<Self> {
            anyhow::bail!("an infallible stream cannot return a business error")
        }
    }
    // SAFETY: the return ABI agrees with Lower; there is no possible return value.
    unsafe impl<UT> LowerReturn<UT> for Infallible {
        type ReturnType = RustBuffer;
        fn lower_return(value: Self) -> Result<RustBuffer, RustCallError> {
            match value {}
        }
    }
    // Unexpected host failures remain framework failures, never an impossible E.
    impl<UT> crate::ConvertError<UT> for Infallible {
        fn try_convert_unexpected_callback_error(
            error: crate::UnexpectedUniFFICallbackError,
        ) -> crate::Result<Self> {
            Err(error.into())
        }
    }
    // SAFETY: no business error can inhabit this type.
    unsafe impl<UT> crate::LowerError<UT> for Infallible {
        fn lower_error(value: Self) -> RustBuffer {
            match value {}
        }
    }
    impl<UT> TypeId<UT> for Infallible {
        const TYPE_ID_META: MetadataBuffer =
            MetadataBuffer::from_code(crate::metadata::codes::TYPE_UNIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Lift, Lower};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{mpsc, Barrier, Weak};
    use std::task::{Wake, Waker};
    use std::time::Duration;

    struct AlwaysPending;

    impl Stream for AlwaysPending {
        type Item = Result<u32, String>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    struct GatedPending {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        first_poll: bool,
    }

    impl Stream for GatedPending {
        type Item = Result<u32, String>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.first_poll {
                self.first_poll = false;
                self.entered.wait();
                self.release.wait();
            }
            Poll::Pending
        }
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    static PENDING_REGISTRY: RustStreamRegistry<u32, String> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    static OPTIONAL_STREAM_REGISTRY: RustStreamRegistry<Option<u32>, String> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    struct OptionalItemSequence {
        values: std::collections::VecDeque<Result<Option<u32>, String>>,
    }

    impl Stream for OptionalItemSequence {
        type Item = Result<Option<u32>, String>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.values.pop_front())
        }
    }

    struct CancelOwnerOnWake {
        owner: Arc<RustStream<u32, String>>,
        calls: Arc<AtomicUsize>,
    }

    struct ProbeRegistryOnWake {
        available: Arc<AtomicUsize>,
    }

    impl Wake for ProbeRegistryOnWake {
        fn wake(self: Arc<Self>) {
            let available = PENDING_REGISTRY.try_lock().is_ok();
            self.available
                .store(if available { 1 } else { 2 }, AtomicOrdering::SeqCst);
        }
    }

    impl Wake for CancelOwnerOnWake {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.owner.cancel();
        }
    }

    struct CancelOwnerOnWakerDrop {
        owner: Weak<RustStream<u32, String>>,
        calls: Arc<AtomicUsize>,
    }

    impl Wake for CancelOwnerOnWakerDrop {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for CancelOwnerOnWakerDrop {
        fn drop(&mut self) {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(owner) = self.owner.upgrade() {
                owner.cancel();
            }
        }
    }

    struct ReentrantPollAndDropStream {
        owner: Arc<Mutex<Option<Weak<RustStream<u32, String>>>>>,
        poll_calls: Arc<AtomicUsize>,
        drop_calls: Arc<AtomicUsize>,
    }

    impl Stream for ReentrantPollAndDropStream {
        type Item = Result<u32, String>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.poll_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(owner) = self.owner.lock().unwrap().as_ref().and_then(Weak::upgrade) {
                owner.cancel();
            }
            Poll::Pending
        }
    }

    impl Drop for ReentrantPollAndDropStream {
        fn drop(&mut self) {
            self.drop_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(owner) = self.owner.lock().unwrap().as_ref().and_then(Weak::upgrade) {
                owner.cancel();
            }
        }
    }

    struct CancelRegistryOnDropStream {
        handle: Arc<Mutex<Option<Handle>>>,
    }

    impl Stream for CancelRegistryOnDropStream {
        type Item = Result<u32, String>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    impl Drop for CancelRegistryOnDropStream {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.lock().unwrap().clone() {
                rust_stream_cancel(&PENDING_REGISTRY, handle);
            }
        }
    }

    fn completes_within_timeout(action: impl FnOnce() + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            action();
            sender.send(()).unwrap();
        });
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => worker.join().unwrap(),
            Err(mpsc::RecvTimeoutError::Disconnected) => worker.join().unwrap(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("re-entrant stream callback deadlocked for more than two seconds")
            }
        }
    }

    #[test]
    fn stream_step_tags_preserve_optional_items_and_typed_errors() {
        // A stream step has its own discriminator. `Item(None)` retains the
        // nested Optional tag and can never be confused with `Done`.
        assert_eq!(
            <UniFfiStreamStep<Option<u32>, String> as Lower<crate::UniFfiTag>>::lower(
                UniFfiStreamStep::Done,
            )
            .destroy_into_vec(),
            vec![UNIFFI_STREAM_STEP_DONE_TAG]
        );
        assert_eq!(
            <UniFfiStreamStep<Option<u32>, String> as Lower<crate::UniFfiTag>>::lower(
                UniFfiStreamStep::Item(None),
            )
            .destroy_into_vec(),
            vec![UNIFFI_STREAM_STEP_ITEM_TAG, 0]
        );
        assert_eq!(
            <UniFfiStreamStep<Option<u32>, String> as Lower<crate::UniFfiTag>>::lower(
                UniFfiStreamStep::Item(Some(1)),
            )
            .destroy_into_vec(),
            vec![UNIFFI_STREAM_STEP_ITEM_TAG, 1, 0, 0, 0, 1]
        );
        let encoded_error =
            <UniFfiStreamStep<Option<u32>, String> as Lower<crate::UniFfiTag>>::lower(
                UniFfiStreamStep::Error("boom".to_owned()),
            );
        assert_eq!(
            <UniFfiStreamStep<Option<u32>, String> as Lift<crate::UniFfiTag>>::try_lift(
                encoded_error,
            )
            .unwrap(),
            UniFfiStreamStep::Error("boom".to_owned())
        );
        assert!(
            <UniFfiStreamStep<Option<u32>, String> as Lift<crate::UniFfiTag>>::try_lift(
                RustBuffer::from_vec(vec![0]),
            )
            .unwrap_err()
            .to_string()
            .contains("unexpected UniFFI output stream step tag")
        );
        assert!(
            <UniFfiStreamStep<Option<u32>, String> as Lift<crate::UniFfiTag>>::try_lift(
                RustBuffer::from_vec(vec![UNIFFI_STREAM_STEP_DONE_TAG, 0]),
            )
            .is_err(),
            "Done must reject a payload rather than accepting a nullable EOF fallback"
        );

        let handle = rust_stream_new(
            &OPTIONAL_STREAM_REGISTRY,
            Box::pin(OptionalItemSequence {
                values: [Ok(Some(1)), Ok(None), Err("boom".to_owned())]
                    .into_iter()
                    .collect(),
            }),
        );
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let mut first = Box::pin(rust_stream_next_async(
            &OPTIONAL_STREAM_REGISTRY,
            handle.clone(),
        ));
        assert!(matches!(
            first.as_mut().poll(&mut cx),
            Poll::Ready(Ok(UniFfiStreamStep::Item(Some(1))))
        ));
        let mut optional_item = Box::pin(rust_stream_next_async(
            &OPTIONAL_STREAM_REGISTRY,
            handle.clone(),
        ));
        assert!(matches!(
            optional_item.as_mut().poll(&mut cx),
            Poll::Ready(Ok(UniFfiStreamStep::Item(None)))
        ));
        let mut error = Box::pin(rust_stream_next_async(
            &OPTIONAL_STREAM_REGISTRY,
            handle.clone(),
        ));
        assert!(matches!(
            error.as_mut().poll(&mut cx),
            Poll::Ready(Ok(UniFfiStreamStep::Error(message))) if message == "boom"
        ));
        assert!(
            !OPTIONAL_STREAM_REGISTRY
                .lock()
                .unwrap()
                .contains_key(&handle.as_raw()),
            "an Error is terminal but is not a Done value"
        );
    }

    #[test]
    fn cancelling_pending_next_wakes_and_completes_cancelled() {
        let handle = rust_stream_new(&PENDING_REGISTRY, Box::pin(AlwaysPending));
        let mut next = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, handle.clone()));
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Pending));
        rust_stream_cancel(&PENDING_REGISTRY, handle.clone());
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
        assert!(matches!(
            next.as_mut().poll(&mut cx),
            Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
        ));
        assert!(!PENDING_REGISTRY
            .lock()
            .unwrap()
            .contains_key(&handle.as_raw()));

        // Repeated cancellation is idempotent and never emits a second wake.
        rust_stream_cancel(&PENDING_REGISTRY, handle);
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn concurrent_next_lift_error_preserves_registry_for_pending_cancel() {
        let handle = rust_stream_new(&PENDING_REGISTRY, Box::pin(AlwaysPending));
        let mut first = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, handle.clone()));
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));

        let mut concurrent = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, handle.clone()));
        assert!(matches!(
            concurrent.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(PENDING_REGISTRY
            .lock()
            .unwrap()
            .contains_key(&handle.as_raw()));

        rust_stream_cancel(&PENDING_REGISTRY, handle.clone());
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
        assert!(matches!(
            first.as_mut().poll(&mut cx),
            Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
        ));
        assert!(!PENDING_REGISTRY
            .lock()
            .unwrap()
            .contains_key(&handle.as_raw()));
    }

    #[test]
    fn cancel_wakes_after_releasing_registry_mutex() {
        let handle = rust_stream_new(&PENDING_REGISTRY, Box::pin(AlwaysPending));
        let mut next = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, handle.clone()));
        let available = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(ProbeRegistryOnWake {
            available: available.clone(),
        }));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Pending));
        rust_stream_cancel(&PENDING_REGISTRY, handle);
        assert_eq!(available.load(AtomicOrdering::SeqCst), 1);
        assert!(matches!(
            next.as_mut().poll(&mut cx),
            Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
        ));
    }

    #[test]
    fn wake_and_waker_drop_can_reenter_stream_state_without_deadlock() {
        completes_within_timeout(|| {
            let owner = Arc::new(RustStream::new(Box::pin(AlwaysPending)));
            let wake_calls = Arc::new(AtomicUsize::new(0));
            let mut next = Box::pin(owner.clone().begin_next());
            let waker = Waker::from(Arc::new(CancelOwnerOnWake {
                owner: owner.clone(),
                calls: wake_calls.clone(),
            }));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(next.as_mut().poll(&mut cx), Poll::Pending));
            owner.cancel();
            assert_eq!(wake_calls.load(AtomicOrdering::SeqCst), 1);
            assert!(matches!(
                next.as_mut().poll(&mut cx),
                Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
            ));

            let owner = Arc::new(RustStream::new(Box::pin(AlwaysPending)));
            let drop_calls = Arc::new(AtomicUsize::new(0));
            let mut next = Box::pin(owner.clone().begin_next());
            {
                let waker = Waker::from(Arc::new(CancelOwnerOnWakerDrop {
                    owner: Arc::downgrade(&owner),
                    calls: drop_calls.clone(),
                }));
                let mut cx = Context::from_waker(&waker);
                assert!(matches!(next.as_mut().poll(&mut cx), Poll::Pending));
            }
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            assert!(matches!(
                next.as_mut().poll(&mut cx),
                Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
            ));
            assert_eq!(drop_calls.load(AtomicOrdering::SeqCst), 1);
        });
    }

    #[test]
    fn user_stream_poll_and_drop_can_reenter_state_without_deadlock() {
        completes_within_timeout(|| {
            let owner_slot = Arc::new(Mutex::new(None));
            let poll_calls = Arc::new(AtomicUsize::new(0));
            let drop_calls = Arc::new(AtomicUsize::new(0));
            let owner = Arc::new(RustStream::new(Box::pin(ReentrantPollAndDropStream {
                owner: owner_slot.clone(),
                poll_calls: poll_calls.clone(),
                drop_calls: drop_calls.clone(),
            })));
            *owner_slot.lock().unwrap() = Some(Arc::downgrade(&owner));
            let mut next = Box::pin(owner.clone().begin_next());
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            assert!(matches!(
                next.as_mut().poll(&mut cx),
                Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
            ));
            assert_eq!(poll_calls.load(AtomicOrdering::SeqCst), 1);
            assert_eq!(drop_calls.load(AtomicOrdering::SeqCst), 1);
        });
    }

    #[test]
    fn stream_drop_can_reenter_raw_cancel_without_registry_deadlock() {
        completes_within_timeout(|| {
            let handle_slot = Arc::new(Mutex::new(None));
            let handle = rust_stream_new(
                &PENDING_REGISTRY,
                Box::pin(CancelRegistryOnDropStream {
                    handle: handle_slot.clone(),
                }),
            );
            *handle_slot.lock().unwrap() = Some(handle.clone());
            let mut next = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, handle.clone()));
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            assert!(matches!(
                next.as_mut().poll(&mut cx),
                Poll::Ready(Ok(UniFfiStreamStep::Done))
            ));
            assert!(!PENDING_REGISTRY
                .lock()
                .unwrap()
                .contains_key(&handle.as_raw()));
        });
    }

    #[test]
    fn cancel_before_poll_and_dropped_pending_next_release_state() {
        let before_poll = rust_stream_new(&PENDING_REGISTRY, Box::pin(AlwaysPending));
        let mut next = Box::pin(rust_stream_next_async(
            &PENDING_REGISTRY,
            before_poll.clone(),
        ));
        rust_stream_cancel(&PENDING_REGISTRY, before_poll);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(matches!(
            next.as_mut().poll(&mut cx),
            Poll::Ready(Ok(UniFfiStreamStep::Done))
        ));

        let dropped = rust_stream_new(&PENDING_REGISTRY, Box::pin(AlwaysPending));
        let mut first = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, dropped.clone()));
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));
        drop(first);
        let mut second = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, dropped.clone()));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));
        rust_stream_cancel(&PENDING_REGISTRY, dropped);
        assert!(matches!(
            second.as_mut().poll(&mut cx),
            Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
        ));
    }

    #[test]
    fn cancel_racing_with_pending_poll_has_no_lost_wake() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let handle = rust_stream_new(
            &PENDING_REGISTRY,
            Box::pin(GatedPending {
                entered: entered.clone(),
                release: release.clone(),
                first_poll: true,
            }),
        );
        let counter = Arc::new(WakeCounter::default());
        let thread_counter = counter.clone();
        let thread_handle = handle.clone();
        let (first_poll_sender, first_poll_receiver) = mpsc::channel();
        let (resume_sender, resume_receiver) = mpsc::channel();
        let poller = std::thread::spawn(move || {
            let mut next = Box::pin(rust_stream_next_async(&PENDING_REGISTRY, thread_handle));
            let waker = Waker::from(thread_counter);
            let mut cx = Context::from_waker(&waker);
            match next.as_mut().poll(&mut cx) {
                Poll::Pending => {
                    first_poll_sender.send(false).unwrap();
                    resume_receiver
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    assert!(matches!(
                        next.as_mut().poll(&mut cx),
                        Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>()
                    ));
                }
                Poll::Ready(Err(error)) if error.error.is::<StreamCancelled>() => {
                    // Cancellation won the race while user poll code was
                    // gated, so this very poll observed Closed and settled.
                    first_poll_sender.send(true).unwrap();
                }
                other => panic!("unexpected first racing poll result: {other:?}"),
            }
        });

        entered.wait();
        let cancel_started = Arc::new(Barrier::new(2));
        let cancel_thread_started = cancel_started.clone();
        let canceller = std::thread::spawn(move || {
            cancel_thread_started.wait();
            rust_stream_cancel(&PENDING_REGISTRY, handle);
        });
        cancel_started.wait();
        release.wait();
        let completed_in_first_poll = first_poll_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        canceller.join().unwrap();
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
        if !completed_in_first_poll {
            resume_sender.send(()).unwrap();
        }
        poller.join().unwrap();
    }
}
