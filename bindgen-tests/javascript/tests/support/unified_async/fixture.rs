//! One business crate compiled unchanged for native and wasm32.
#![deny(warnings)]

pub mod local;
pub mod actor;

use std::cell::Cell;
use std::collections::HashMap;
use std::future::{pending, poll_fn, Future};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use uniffi::async_runtime::{current_context, sleep, spawn, timeout, TaskError};
use uniffi::deps::futures_core::Stream;

#[derive(Clone, Default, Debug, uniffi::Record)]
pub struct Snapshot {
    pub started: u32,
    pub polls: u32,
    pub dropped: u32,
    pub cleaning: u32,
    pub nested: u32,
    pub cleaned: u32,
    pub produced: u32,
    pub owner_dropped: u32,
}

#[derive(Default)]
struct Probe {
    counts: Snapshot,
    gates: u32,
    waiters: Vec<Waker>,
}

fn probes() -> &'static Mutex<HashMap<u32, Probe>> {
    static PROBES: OnceLock<Mutex<HashMap<u32, Probe>>> = OnceLock::new();
    PROBES.get_or_init(Mutex::default)
}

fn update(id: u32, f: impl FnOnce(&mut Snapshot)) {
    f(&mut probes().lock().unwrap().entry(id).or_default().counts);
}

#[uniffi::export]
pub fn snapshot(id: u32) -> Snapshot {
    probes()
        .lock()
        .unwrap()
        .entry(id)
        .or_default()
        .counts
        .clone()
}

/// 1 = business completion, 2 = outer cleanup, 4 = nested cleanup.
#[uniffi::export]
pub fn open_gate(id: u32, gate: u32) {
    let waiters = {
        let mut probes = probes().lock().unwrap();
        let probe = probes.entry(id).or_default();
        probe.gates |= gate;
        std::mem::take(&mut probe.waiters)
    };
    for waker in waiters {
        waker.wake();
    }
}

async fn gate(id: u32, bit: u32, business: bool) {
    poll_fn(|cx| {
        let mut probes = probes().lock().unwrap();
        let probe = probes.entry(id).or_default();
        if business {
            probe.counts.polls += 1;
        }
        if probe.gates & bit != 0 {
            Poll::Ready(())
        } else {
            if !probe.waiters.iter().any(|w| w.will_wake(cx.waker())) {
                probe.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        }
    })
    .await;
}

struct NestedCleanup(u32);
impl Drop for NestedCleanup {
    fn drop(&mut self) {
        let id = self.0;
        update(id, |s| s.nested += 1);
        current_context()
            .expect("nested cleanup destructor context")
            .defer_cleanup(async move {
                gate(id, 4, false).await;
                update(id, |s| s.cleaned += 1);
            })
            .unwrap();
    }
}

struct CleanupOnDrop(u32);
impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        let id = self.0;
        update(id, |s| s.dropped += 1);
        current_context()
            .expect("business destructor context")
            .defer_cleanup(async move {
                update(id, |s| s.cleaning += 1);
                let nested = NestedCleanup(id);
                gate(id, 2, false).await;
                drop(nested);
            })
            .unwrap();
    }
}

#[derive(uniffi::Object)]
pub struct CancelledResultObject {
    id: u32,
    cleanup: bool,
}

impl Drop for CancelledResultObject {
    fn drop(&mut self) {
        if self.cleanup {
            drop(CleanupOnDrop(self.id));
        } else {
            update(self.id, |s| s.dropped += 1);
        }
    }
}

#[uniffi::export]
pub async fn cancel_with_object(id: u32, cleanup: bool) -> Arc<CancelledResultObject> {
    let object = Arc::new(CancelledResultObject { id, cleanup });
    current_context()
        .expect("return object call context")
        .cancel();
    object
}

#[uniffi::export]
pub async fn ready(value: u32) -> u32 {
    value
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct Payload {
    pub value: u32,
    // A business field called signal must never be mistaken for CallOptions.
    pub signal: String,
}

#[uniffi::export]
pub async fn echo_payload(payload: Payload) -> Payload {
    pending_value(payload.value).await;
    payload
}

// Cell is Send but !Sync. On wasm the Rc additionally makes this future !Send.
// Both values survive an actual Pending poll and are used after the await.
#[uniffi::export]
pub async fn pending_value(value: u32) -> u32 {
    let cell = Cell::new(value);
    #[cfg(target_arch = "wasm32")]
    let local = std::rc::Rc::new(value);
    let mut first = true;
    poll_fn(|cx| {
        if std::mem::take(&mut first) {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    #[cfg(target_arch = "wasm32")]
    assert_eq!(*local, cell.get());
    cell.get()
}

#[uniffi::export]
pub async fn sleep_value(value: u32, millis: u32) -> u32 {
    sleep(Duration::from_millis(millis.into())).await;
    value
}

#[uniffi::export]
pub async fn timer_checks() -> bool {
    sleep(Duration::ZERO).await;
    let ready = timeout(Duration::ZERO, async { 7 }).await == Ok(7);
    let expired = timeout(Duration::from_millis(2), pending::<()>())
        .await
        .is_err();
    // Poll Duration::MAX without waiting centuries; exercise timer chunking and drop.
    let long = timeout(Duration::from_millis(2), sleep(Duration::MAX))
        .await
        .is_err();
    ready && expired && long
}

#[uniffi::export]
pub async fn guarded(id: u32, value: u32) -> u32 {
    update(id, |s| s.started += 1);
    let _resource = CleanupOnDrop(id);
    gate(id, 1, true).await;
    value
}

#[uniffi::export]
pub async fn timeout_guarded(id: u32) -> bool {
    timeout(Duration::from_millis(5), guarded(id, 0))
        .await
        .is_err()
}

#[uniffi::export]
pub async fn spawn_value(value: u32) -> u32 {
    let state = Cell::new(value);
    #[cfg(target_arch = "wasm32")]
    let local = std::rc::Rc::new(value);
    spawn(async move {
        sleep(Duration::from_millis(1)).await;
        #[cfg(target_arch = "wasm32")]
        assert_eq!(*local, state.get());
        state
    })
    .wait()
    .await
    .unwrap()
    .get()
}

#[uniffi::export]
pub async fn cancel_spawn(id: u32) -> bool {
    let task = spawn(guarded(id, 0));
    while snapshot(id).started == 0 {
        sleep(Duration::from_millis(1)).await;
    }
    task.cancel();
    task.cancel();
    task.cancel_and_wait().await == Err(TaskError::Cancelled)
}

#[uniffi::export]
pub async fn detach_spawn(id: u32) {
    drop(spawn(async move {
        gate(id, 1, false).await;
        update(id, |s| s.cleaned += 1);
    }));
}

#[derive(Clone, Debug, uniffi::Error)]
pub enum FixtureError {
    Boom,
}
impl std::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fixture boom")
    }
}
impl std::error::Error for FixtureError {}

struct ProbeStream {
    id: u32,
    mode: u32,
    cursor: u32,
}
impl Drop for ProbeStream {
    fn drop(&mut self) {
        update(self.id, |s| s.dropped += 1);
    }
}
impl Stream for ProbeStream {
    type Item = Result<Option<u32>, FixtureError>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        update(self.id, |s| s.polls += 1);
        // Never wakes: return() must cancel an actually pending next().
        if self.mode == 2 {
            return Poll::Pending;
        }
        let cursor = self.cursor;
        self.cursor += 1;
        let item = match (self.mode, cursor) {
            (3, _) => None,
            (1, 1) => Some(Err(FixtureError::Boom)),
            (_, 0) => Some(Ok(Some(7))),
            (_, 1) => Some(Ok(None)),
            (_, 2) => Some(Ok(Some(9))),
            _ => None,
        };
        if item.is_some() {
            update(self.id, |s| s.produced += 1);
        }
        Poll::Ready(item)
    }
}

// These helpers intentionally adapt legal Rust declarations into supported
// exported async fn / UniFfiStream signatures. They do NOT assert that macros
// directly support impl Future, RPITIT, or an async stream factory declaration.
fn impl_future(value: u32) -> impl Future<Output = u32> + Send {
    async move {
        let mut first = true;
        poll_fn(|cx| {
            if std::mem::take(&mut first) {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(value)
            }
        })
        .await
    }
}
trait Calculator {
    fn calculate(&self, value: u32) -> impl Future<Output = u32> + Send;
}
struct CalculatorImpl;
impl Calculator for CalculatorImpl {
    fn calculate(&self, value: u32) -> impl Future<Output = u32> + Send {
        impl_future(value)
    }
}
fn impl_stream(id: u32, mode: u32) -> impl Stream<Item = Result<Option<u32>, FixtureError>> + Send {
    update(id, |s| s.started += 1);
    ProbeStream {
        id,
        mode,
        cursor: 0,
    }
}
async fn async_stream_factory(
    id: u32,
    mode: u32,
) -> impl Stream<Item = Result<Option<u32>, FixtureError>> + Send {
    sleep(Duration::ZERO).await;
    impl_stream(id, mode)
}

#[uniffi::export]
pub async fn declaration_probe(value: u32) -> u32 {
    CalculatorImpl.calculate(impl_future(value).await).await
}

#[uniffi::export]
pub fn events(id: u32, mode: u32) -> uniffi::UniFfiStream<Option<u32>, FixtureError> {
    Box::pin(impl_stream(id, mode))
}

// A real lazy adapter awaits the factory on first demand, then forwards pulls.
#[uniffi::export]
pub fn factory_events(id: u32) -> uniffi::UniFfiStream<Option<u32>, FixtureError> {
    let mut factory = Some(Box::pin(async_stream_factory(id, 0)));
    let mut stream = None;
    Box::pin(futures_util::stream::poll_fn(move |cx| {
        if let Some(future) = factory.as_mut() {
            match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(value) => {
                    stream = Some(Box::pin(value));
                    factory = None;
                }
            }
        }
        stream.as_mut().unwrap().as_mut().poll_next(cx)
    }))
}

uniffi::setup_scaffolding!();
