//! Shared native-language async contract fixture. Enabled only by this round's
//! Swift/JNA/JNI tests; deferred language backends do not receive these types.

use std::{
    cell::Cell,
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context, Poll},
    thread::ThreadId,
};
use uniffi::{
    async_runtime::{
        current_context, BoxFuture, CancellationToken, LocalExecutor, LocalObject, SendOutsideWasm,
    },
    deps::{
        futures_core::Stream,
        futures_util::{stream, StreamExt},
    },
    UniFfiInputStream, UniFfiStream,
};

pub mod lifecycle;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UnifiedError {
    #[error("factory failed")]
    Factory,
    #[error("item failed")]
    Item,
    #[error("owner closed")]
    Closed,
}

#[derive(uniffi::Object)]
pub struct UnifiedGate {
    token: CancellationToken,
}

#[uniffi::export]
impl UnifiedGate {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            token: CancellationToken::new(),
        })
    }
    pub fn open(&self) {
        self.token.cancel();
    }
    pub fn is_open(&self) -> bool {
        self.token.is_cancelled()
    }
}
impl UnifiedGate {
    async fn wait(&self) {
        self.token.cancelled().await;
    }
}

#[derive(uniffi::Record)]
pub struct UnifiedCounts {
    pub started: u32,
    pub polls: u32,
    pub dropped: u32,
    pub cleaned: u32,
}

#[derive(Default, uniffi::Object)]
pub struct UnifiedTracker {
    started: AtomicU32,
    polls: AtomicU32,
    dropped: AtomicU32,
    cleaned: AtomicU32,
}
#[uniffi::export]
impl UnifiedTracker {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn counts(&self) -> UnifiedCounts {
        UnifiedCounts {
            started: self.started.load(Ordering::SeqCst),
            polls: self.polls.load(Ordering::SeqCst),
            dropped: self.dropped.load(Ordering::SeqCst),
            cleaned: self.cleaned.load(Ordering::SeqCst),
        }
    }
}
struct TrackedDrop {
    tracker: Arc<UnifiedTracker>,
    gate: Arc<UnifiedGate>,
}

struct TokioDrop {
    tracker: Arc<UnifiedTracker>,
    gate: Arc<UnifiedGate>,
}
impl Drop for TokioDrop {
    fn drop(&mut self) {
        assert!(tokio::runtime::Handle::try_current().is_ok());
        self.tracker.dropped.fetch_add(1, Ordering::SeqCst);
        let tracker = self.tracker.clone();
        let gate = self.gate.clone();
        current_context()
            .unwrap()
            .defer_cleanup(async move {
                assert!(tokio::runtime::Handle::try_current().is_ok());
                gate.wait().await;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                assert!(tokio::runtime::Handle::try_current().is_ok());
                tracker.cleaned.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
    }
}

#[uniffi::export]
pub async fn unified_runtime_ready(tracker: Arc<UnifiedTracker>, cleanup: Arc<UnifiedGate>) -> u32 {
    assert!(tokio::runtime::Handle::try_current().is_ok());
    let _guard = TokioDrop {
        tracker: tracker.clone(),
        gate: cleanup,
    };
    tracker.started.fetch_add(1, Ordering::SeqCst);
    47
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn unified_tokio_pending(
    tracker: Arc<UnifiedTracker>,
    body: Arc<UnifiedGate>,
    cleanup: Arc<UnifiedGate>,
) -> u32 {
    assert!(tokio::runtime::Handle::try_current().is_ok());
    let _guard = TokioDrop {
        tracker: tracker.clone(),
        gate: cleanup,
    };
    tracker.started.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    body.wait().await;
    47
}

#[uniffi::export]
pub fn unified_tokio_values(
    tracker: Arc<UnifiedTracker>,
    cleanup: Arc<UnifiedGate>,
) -> UniFfiStream<u32, UnifiedError> {
    let guard = TokioDrop {
        tracker: tracker.clone(),
        gate: cleanup,
    };
    let mut timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut yielded = false;
    Box::pin(stream::poll_fn(move |cx| {
        let _guard = &guard;
        assert!(tokio::runtime::Handle::try_current().is_ok());
        tracker.polls.fetch_add(1, Ordering::SeqCst);
        if yielded {
            return Poll::Ready(None);
        }
        let timer = timer.get_or_insert_with(|| {
            tracker.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1)))
        });
        match timer.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                yielded = true;
                Poll::Ready(Some(Ok(47)))
            }
        }
    }))
}
impl Drop for TrackedDrop {
    fn drop(&mut self) {
        self.tracker.dropped.fetch_add(1, Ordering::SeqCst);
        let tracker = self.tracker.clone();
        let gate = self.gate.clone();
        current_context()
            .expect("owned resource Drop must have a call context")
            .defer_cleanup(async move {
                gate.wait().await;
                tracker.cleaned.fetch_add(1, Ordering::SeqCst);
            })
            .expect("cleanup registration must precede acknowledgement");
    }
}

#[uniffi::export]
pub async fn unified_ready(value: u32) -> u32 {
    value
}

#[uniffi::export]
pub fn unified_future(value: u32) -> impl Future<Output = u32> {
    async move { value }
}

#[uniffi::export]
pub fn unified_boxed(value: u32) -> BoxFuture<'static, u32> {
    Box::pin(async move { value })
}

#[uniffi::export]
pub async fn unified_pending(
    tracker: Arc<UnifiedTracker>,
    body: Arc<UnifiedGate>,
    cleanup: Arc<UnifiedGate>,
) -> u32 {
    let _drop = TrackedDrop {
        tracker: tracker.clone(),
        gate: cleanup,
    };
    tracker.started.fetch_add(1, Ordering::SeqCst);
    body.wait().await;
    7
}

#[uniffi::export]
pub fn unified_infallible(value: u32) -> Result<u32, Infallible> {
    Ok(value)
}

#[uniffi::export]
pub async fn unified_infallible_unit() -> Result<(), Infallible> {
    Ok(())
}

struct Values {
    remaining: u32,
    index: u32,
    fail: bool,
    tracker: Arc<UnifiedTracker>,
    _drop: TrackedDrop,
}
impl Stream for Values {
    type Item = Result<Option<u32>, UnifiedError>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.tracker.polls.fetch_add(1, Ordering::SeqCst);
        if self.fail && self.index == 1 {
            return Poll::Ready(Some(Err(UnifiedError::Item)));
        }
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        self.remaining -= 1;
        let item = if self.index == 0 {
            None
        } else {
            Some(self.index)
        };
        self.index += 1;
        Poll::Ready(Some(Ok(item)))
    }
}

#[uniffi::export]
pub fn unified_values(
    count: u32,
    fail: bool,
    tracker: Arc<UnifiedTracker>,
    cleanup: Arc<UnifiedGate>,
) -> impl Stream<Item = Result<Option<u32>, UnifiedError>> + SendOutsideWasm {
    tracker.started.fetch_add(1, Ordering::SeqCst);
    Values {
        remaining: count,
        index: 0,
        fail,
        tracker: tracker.clone(),
        _drop: TrackedDrop {
            tracker,
            gate: cleanup,
        },
    }
}

#[uniffi::export]
pub fn unified_plain_values(count: u32) -> impl Stream<Item = u32> + SendOutsideWasm {
    stream::iter(0..count)
}

#[uniffi::export]
pub fn unified_canonical_values(count: u32) -> UniFfiStream<u32, Infallible> {
    Box::pin(stream::iter((0..count).map(Ok)))
}

#[uniffi::export]
pub async fn unified_values_factory(
    count: u32,
    fail: bool,
    body: Arc<UnifiedGate>,
    tracker: Arc<UnifiedTracker>,
    cleanup: Arc<UnifiedGate>,
) -> Result<UniFfiStream<Option<u32>, UnifiedError>, UnifiedError> {
    body.wait().await;
    if fail {
        return Err(UnifiedError::Factory);
    }
    Ok(Box::pin(unified_values(count, false, tracker, cleanup)))
}

#[derive(uniffi::Object)]
pub struct UnifiedService;

// Keep an exported impl in a different module from its type. Runtime helpers
// belong to the declaration module, just like the metadata's module_path.
pub mod runtime_placement {
    use super::UnifiedService;

    #[uniffi::export(async_runtime = "tokio")]
    impl UnifiedService {
        pub async fn runtime_active(&self) -> bool {
            tokio::runtime::Handle::try_current().is_ok()
        }
    }
}
#[uniffi::export]
impl UnifiedService {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self
    }
    #[uniffi::constructor]
    pub async fn create() -> Result<Self, Infallible> {
        Ok(Self)
    }
    pub fn future(&self, value: u32) -> BoxFuture<'_, u32> {
        Box::pin(async move { value })
    }
    pub fn values(&self, count: u32) -> impl Stream<Item = u32> + SendOutsideWasm {
        stream::iter(0..count)
    }
    pub async fn values_async(&self, count: u32) -> UniFfiStream<u32, Infallible> {
        unified_canonical_values(count)
    }
    pub fn echo(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> UniFfiStream<u32, UnifiedError> {
        Box::pin(source)
    }
    pub async fn echo_async(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> UniFfiStream<u32, UnifiedError> {
        Box::pin(source)
    }
    pub async fn sum(
        &self,
        mut source: UniFfiInputStream<u32, UnifiedError>,
    ) -> Result<u32, UnifiedError> {
        let mut sum = 0;
        while let Some(value) = source.next().await {
            sum += value?;
        }
        Ok(sum)
    }
}

#[uniffi::export(rust, foreign)]
pub trait UnifiedHost: SendOutsideWasm + uniffi::async_runtime::SyncOutsideWasm {
    async fn value(&self, value: u32) -> u32;
    fn boxed(&self, value: u32) -> BoxFuture<'_, u32>;
    fn values(&self, count: u32) -> UniFfiStream<u32, UnifiedError>;
    async fn produce(&self, count: u32) -> Result<UniFfiStream<u32, UnifiedError>, UnifiedError>;
    async fn consume(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> Result<u32, UnifiedError>;
    fn echo(&self, source: UniFfiInputStream<u32, UnifiedError>)
        -> UniFfiStream<u32, UnifiedError>;
    async fn echo_async(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> UniFfiStream<u32, UnifiedError>;
}

#[derive(uniffi::Object)]
pub struct UnifiedRustHost;
#[uniffi::export]
impl UnifiedHost for UnifiedRustHost {
    async fn value(&self, value: u32) -> u32 {
        value
    }
    fn boxed(&self, value: u32) -> BoxFuture<'_, u32> {
        Box::pin(async move { value })
    }
    fn values(&self, count: u32) -> UniFfiStream<u32, UnifiedError> {
        Box::pin(stream::iter((0..count).map(Ok)))
    }
    async fn produce(&self, count: u32) -> Result<UniFfiStream<u32, UnifiedError>, UnifiedError> {
        if count == 99 {
            return Err(UnifiedError::Factory);
        }
        Ok(self.values(count))
    }
    async fn consume(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> Result<u32, UnifiedError> {
        UnifiedService.sum(source).await
    }
    fn echo(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> UniFfiStream<u32, UnifiedError> {
        Box::pin(source)
    }
    async fn echo_async(
        &self,
        source: UniFfiInputStream<u32, UnifiedError>,
    ) -> UniFfiStream<u32, UnifiedError> {
        Box::pin(source)
    }
}

#[uniffi::export]
pub fn unified_rust_host() -> Arc<dyn UnifiedHost> {
    Arc::new(UnifiedRustHost)
}
#[uniffi::export]
pub fn unified_host_echo(
    host: Arc<dyn UnifiedHost>,
    source: UniFfiInputStream<u32, UnifiedError>,
) -> UniFfiStream<u32, UnifiedError> {
    host.echo(source)
}
#[uniffi::export]
pub async fn unified_host_echo_async(
    host: Arc<dyn UnifiedHost>,
    source: UniFfiInputStream<u32, UnifiedError>,
) -> UniFfiStream<u32, UnifiedError> {
    host.echo_async(source).await
}

#[uniffi::export]
pub async fn unified_host_value(host: Arc<dyn UnifiedHost>, value: u32) -> u32 {
    host.value(value).await
}
#[uniffi::export]
pub async fn unified_host_boxed(host: Arc<dyn UnifiedHost>, value: u32) -> u32 {
    host.boxed(value).await
}
#[uniffi::export]
pub async fn unified_host_values(
    host: Arc<dyn UnifiedHost>,
    count: u32,
) -> Result<u32, UnifiedError> {
    let mut values = host.values(count);
    let mut sum = 0;
    while let Some(value) = values.next().await {
        sum += value?;
    }
    Ok(sum)
}
#[uniffi::export]
pub async fn unified_host_factory(
    host: Arc<dyn UnifiedHost>,
    count: u32,
) -> Result<u32, UnifiedError> {
    let mut values = host.produce(count).await?;
    let mut sum = 0;
    while let Some(value) = values.next().await {
        sum += value?;
    }
    Ok(sum)
}
#[uniffi::export]
pub async fn unified_host_consume(
    host: Arc<dyn UnifiedHost>,
    source: UniFfiInputStream<u32, UnifiedError>,
) -> Result<u32, UnifiedError> {
    host.consume(source).await
}

#[derive(uniffi::Object)]
pub struct UnifiedOwner {
    executor: LocalExecutor,
}
#[uniffi::export]
impl UnifiedOwner {
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, UnifiedError> {
        Ok(Arc::new(Self {
            executor: LocalExecutor::new(8).map_err(|_| UnifiedError::Closed)?,
        }))
    }
    pub async fn close(&self) -> Result<(), UnifiedError> {
        self.executor
            .close()
            .await
            .map_err(|_| UnifiedError::Closed)
    }
}

struct LocalDrop {
    owner: ThreadId,
    tracker: Arc<UnifiedTracker>,
    cleanup: Arc<UnifiedGate>,
}
impl Drop for LocalDrop {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        self.tracker.dropped.fetch_add(1, Ordering::SeqCst);
        let owner = self.owner;
        let tracker = self.tracker.clone();
        let cleanup = self.cleanup.clone();
        let local = std::rc::Rc::new(Cell::new(false));
        uniffi::async_runtime::defer_local_cleanup(async move {
            cleanup.wait().await;
            assert_eq!(owner, std::thread::current().id());
            local.set(true);
            assert!(local.get());
            tracker.cleaned.fetch_add(1, Ordering::SeqCst);
        })
        .expect("local Drop registers cleanup on its owner");
    }
}

#[derive(uniffi::Object)]
#[uniffi(local)]
pub struct UnifiedLocal {
    value: Cell<u32>,
    owner: ThreadId,
}
#[uniffi::export(local)]
impl UnifiedLocal {
    #[uniffi::constructor]
    pub async fn create(owner: Arc<UnifiedOwner>) -> Result<Arc<LocalObject<Self>>, UnifiedError> {
        Ok(Arc::new(
            owner
                .executor
                .create(|| async {
                    Self {
                        value: Cell::new(0),
                        owner: std::thread::current().id(),
                    }
                })
                .await
                .map_err(|_| UnifiedError::Closed)?,
        ))
    }
    pub async fn increment(&self) -> u32 {
        assert_eq!(self.owner, std::thread::current().id());
        self.value.set(self.value.get() + 1);
        self.value.get()
    }
    pub async fn pending(
        &self,
        tracker: Arc<UnifiedTracker>,
        body: Arc<UnifiedGate>,
        cleanup: Arc<UnifiedGate>,
    ) -> u32 {
        assert_eq!(self.owner, std::thread::current().id());
        let _drop = LocalDrop {
            owner: self.owner,
            tracker: tracker.clone(),
            cleanup,
        };
        tracker.started.fetch_add(1, Ordering::SeqCst);
        body.wait().await;
        self.value.get()
    }
    pub fn pending_values(
        &self,
        tracker: Arc<UnifiedTracker>,
        body: Arc<UnifiedGate>,
        cleanup: Arc<UnifiedGate>,
    ) -> impl Stream<Item = u32> {
        assert_eq!(self.owner, std::thread::current().id());
        let owner = self.owner;
        let dropped = LocalDrop {
            owner,
            tracker: tracker.clone(),
            cleanup,
        };
        tracker.started.fetch_add(1, Ordering::SeqCst);
        let emitted = std::rc::Rc::new(Cell::new(false));
        let mut pending = Box::pin(async move {
            body.wait().await;
        });
        stream::poll_fn(move |cx| {
            let _ = &dropped;
            assert_eq!(owner, std::thread::current().id());
            tracker.polls.fetch_add(1, Ordering::SeqCst);
            if emitted.get() {
                return Poll::Ready(None);
            }
            if pending.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            emitted.set(true);
            Poll::Ready(Some(7))
        })
    }
    pub fn values(&self, count: u32) -> impl Stream<Item = u32> {
        let owner = self.owner;
        let index = std::rc::Rc::new(Cell::new(0));
        stream::poll_fn(move |_| {
            assert_eq!(owner, std::thread::current().id());
            let value = index.get();
            if value == count {
                return Poll::Ready(None);
            }
            index.set(value + 1);
            Poll::Ready(Some(value))
        })
    }
}
impl Drop for UnifiedLocal {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
    }
}
