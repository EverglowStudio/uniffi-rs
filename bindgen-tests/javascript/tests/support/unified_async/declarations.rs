//! Direct S03 declarations: no async/stream adapter hiding the source signature.
use crate::{gate, update, CleanupOnDrop, FixtureError, ProbeStream};
use futures_util::{stream, StreamExt};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use uniffi::deps::futures_core::Stream;
use uniffi::{
    async_runtime::{BoxFuture, SendOutsideWasm, SyncOutsideWasm},
    UniFfiInputStream,
};

#[uniffi::export]
pub fn direct_future(value: u32) -> impl Future<Output = u32> + SendOutsideWasm {
    async move { crate::pending_value(value).await }
}
#[uniffi::export]
pub fn direct_box_future(value: u32) -> BoxFuture<'static, u32> {
    Box::pin(async move { crate::pending_value(value).await })
}
#[uniffi::export(default(value))]
pub fn rust_default(value: u32) -> u32 {
    value
}

fn raw(id: u32, mode: u32) -> ProbeStream {
    update(id, |s| s.started += 1);
    ProbeStream {
        id,
        mode,
        cursor: 0,
    }
}
#[uniffi::export]
pub fn plain_events(id: u32) -> impl Stream<Item = Option<u32>> + Send + 'static {
    raw(id, 0).map(|value| value.expect("plain fixture has no item error"))
}
#[uniffi::export]
pub fn result_events(
    id: u32,
) -> impl Stream<Item = Result<Option<u32>, FixtureError>> + Send + 'static {
    raw(id, 1)
}
#[uniffi::export]
pub fn sync_factory(
    id: u32,
    fail: bool,
) -> Result<impl Stream<Item = Result<Option<u32>, FixtureError>> + Send + 'static, FixtureError> {
    if fail {
        Err(FixtureError::Factory)
    } else {
        Ok(raw(id, 1))
    }
}
#[uniffi::export]
pub async fn async_factory(
    id: u32,
    fail: bool,
) -> Result<impl Stream<Item = Result<Option<u32>, FixtureError>> + Send + 'static, FixtureError> {
    crate::pending_value(0).await;
    sync_factory(id, fail)
}
#[uniffi::export]
pub fn future_factory(
    id: u32,
    fail: bool,
) -> impl Future<
    Output = Result<
        impl Stream<Item = Result<Option<u32>, FixtureError>> + Send + 'static,
        FixtureError,
    >,
> + SendOutsideWasm {
    async move { async_factory(id, fail).await }
}

pub struct CleanupStream {
    id: u32,
    pending: bool,
}
impl Stream for CleanupStream {
    type Item = u32;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u32>> {
        update(self.id, |s| s.polls += 1);
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(Some(5))
        }
    }
}
impl Drop for CleanupStream {
    fn drop(&mut self) {
        drop(CleanupOnDrop(self.id));
    }
}
#[uniffi::export]
pub fn cleanup_events(id: u32) -> impl Stream<Item = u32> + Send + 'static {
    update(id, |s| s.started += 1);
    CleanupStream { id, pending: true }
}
#[uniffi::export]
pub async fn cancelled_factory(id: u32) -> impl Stream<Item = u32> + Send + 'static {
    let stream = CleanupStream { id, pending: false };
    update(id, |s| s.started += 1);
    uniffi::async_runtime::current_context().unwrap().cancel();
    stream
}

#[derive(uniffi::Object)]
pub struct StreamService;
#[uniffi::export]
impl StreamService {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
    pub fn values(&self, id: u32) -> impl Stream<Item = Option<u32>> + Send + 'static {
        plain_events(id)
    }
    pub async fn values_later(&self, id: u32) -> impl Stream<Item = Option<u32>> + Send + 'static {
        plain_events(id)
    }
    pub fn values_future(
        &self,
        id: u32,
    ) -> BoxFuture<'_, uniffi::UniFfiStream<Option<u32>, FixtureError>> {
        Box::pin(async move { Box::pin(raw(id, 0)) as uniffi::UniFfiStream<_, _> })
    }
    pub async fn consume(
        &self,
        mut input: UniFfiInputStream<u32, FixtureError>,
        id: u32,
        take: u32,
    ) -> Result<u32, FixtureError> {
        update(id, |s| s.started += 1);
        let mut sum = 0;
        for _ in 0..take {
            match input.next().await {
                Some(value) => {
                    sum += value?;
                    update(id, |s| s.produced += 1);
                }
                None => break,
            }
        }
        gate(id, 1, false).await;
        Ok(sum)
    }
}

#[uniffi::export(rust, foreign)]
pub trait StreamProvider: SendOutsideWasm + SyncOutsideWasm {
    fn sync_values(&self) -> impl Stream<Item = u32> + SendOutsideWasm + 'static;
    async fn async_values(&self) -> impl Stream<Item = u32> + SendOutsideWasm + 'static;
    fn boxed_values(&self) -> BoxFuture<'_, uniffi::UniFfiStream<u32, FixtureError>>;
    fn static_values(&self) -> BoxFuture<'static, uniffi::UniFfiStream<u32, FixtureError>>;
    fn static_consume(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> BoxFuture<'static, Result<u32, FixtureError>>;
    async fn consume_input(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> Result<u32, FixtureError>;
}
#[derive(uniffi::Object)]
pub struct RustStreamProvider;
#[uniffi::export]
impl RustStreamProvider {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}
#[uniffi::export]
impl StreamProvider for RustStreamProvider {
    fn sync_values(&self) -> impl Stream<Item = u32> + SendOutsideWasm + 'static {
        stream::iter([11, 12])
    }
    async fn async_values(&self) -> impl Stream<Item = u32> + SendOutsideWasm + 'static {
        stream::iter([13, 14])
    }
    fn boxed_values(&self) -> BoxFuture<'_, uniffi::UniFfiStream<u32, FixtureError>> {
        Box::pin(async {
            Box::pin(stream::iter([Ok(15u32), Ok(16u32)])) as uniffi::UniFfiStream<_, _>
        })
    }
    fn static_values(&self) -> BoxFuture<'static, uniffi::UniFfiStream<u32, FixtureError>> {
        Box::pin(async { Box::pin(stream::iter([Ok(17u32)])) as uniffi::UniFfiStream<_, _> })
    }
    fn static_consume(
        &self,
        mut input: UniFfiInputStream<u32, FixtureError>,
    ) -> BoxFuture<'static, Result<u32, FixtureError>> {
        Box::pin(async move { input.next().await.unwrap_or(Ok(0)) })
    }
    async fn consume_input(
        &self,
        mut input: UniFfiInputStream<u32, FixtureError>,
    ) -> Result<u32, FixtureError> {
        let mut sum = 0;
        while let Some(value) = input.next().await {
            sum += value?;
        }
        Ok(sum)
    }
}
#[uniffi::export(callback(
    path = "argument[0]",
    retention = "retained",
    threading = "may_cross_thread",
    reentrancy = "allowed"
))]
pub async fn host_values(
    provider: Arc<dyn StreamProvider>,
    kind: u32,
) -> Result<Vec<u32>, FixtureError> {
    if kind == 0 {
        Ok(provider.sync_values().collect().await)
    } else if kind == 1 {
        Ok(provider.async_values().await.collect().await)
    } else if kind == 3 {
        let future = provider.static_values();
        drop(provider);
        future.await.collect::<Vec<_>>().await.into_iter().collect()
    } else {
        provider
            .boxed_values()
            .await
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }
}
#[uniffi::export(callback(
    path = "argument[0]",
    retention = "retained",
    threading = "may_cross_thread",
    reentrancy = "allowed"
))]
pub async fn host_consume(
    provider: Arc<dyn StreamProvider>,
    input: UniFfiInputStream<u32, FixtureError>,
) -> Result<u32, FixtureError> {
    provider.consume_input(input).await
}

#[uniffi::export(callback(
    path = "argument[0]",
    retention = "retained",
    threading = "may_cross_thread",
    reentrancy = "allowed"
))]
pub async fn host_static_consume(
    provider: Arc<dyn StreamProvider>,
    input: UniFfiInputStream<u32, FixtureError>,
    unpolled: bool,
) -> Result<u32, FixtureError> {
    let future = provider.static_consume(input);
    drop(provider);
    if unpolled {
        drop(future);
        Ok(0)
    } else {
        future.await
    }
}

// R1-01: host infallibility must not erase the original Rust Result shape.
#[uniffi::export]
pub fn infallible_value(value: u32) -> Result<u32, std::convert::Infallible> {
    Ok(value)
}
#[uniffi::export]
pub async fn infallible_async_value(value: u32) -> Result<u32, std::convert::Infallible> {
    Ok(crate::pending_value(value).await)
}
#[uniffi::export]
pub fn infallible_future_value(
    value: u32,
) -> BoxFuture<'static, Result<u32, std::convert::Infallible>> {
    Box::pin(async move { infallible_async_value(value).await })
}
#[uniffi::export]
pub fn canonical_infallible() -> uniffi::UniFfiStream<u32, std::convert::Infallible> {
    Box::pin(stream::iter([Ok(71), Ok(72)]))
}
#[uniffi::export]
pub fn infallible_result_items(
) -> impl Stream<Item = Result<u32, std::convert::Infallible>> + SendOutsideWasm + 'static {
    stream::iter([Ok(73), Ok(74)])
}
#[uniffi::export]
pub fn infallible_sync_factory(
) -> Result<uniffi::UniFfiStream<u32, std::convert::Infallible>, std::convert::Infallible> {
    Ok(canonical_infallible())
}
#[uniffi::export]
pub async fn infallible_async_factory(
) -> Result<impl Stream<Item = u32> + SendOutsideWasm + 'static, std::convert::Infallible> {
    Ok(stream::iter([75, 76]))
}
#[uniffi::export]
pub fn infallible_future_factory() -> BoxFuture<
    'static,
    Result<uniffi::UniFfiStream<u32, std::convert::Infallible>, std::convert::Infallible>,
> {
    Box::pin(async { Ok(canonical_infallible()) })
}

#[uniffi::export(rust, foreign)]
pub trait InfallibleProvider: SendOutsideWasm + SyncOutsideWasm {
    fn unit(&self) -> Result<(), std::convert::Infallible>;
    async fn async_unit(&self) -> Result<(), std::convert::Infallible>;
    fn future_unit(&self) -> BoxFuture<'static, Result<(), std::convert::Infallible>>;
    fn value(&self) -> Result<u32, std::convert::Infallible>;
    async fn async_value(&self) -> Result<u32, std::convert::Infallible>;
    fn future_value(&self) -> BoxFuture<'static, Result<u32, std::convert::Infallible>>;
    fn canonical(&self) -> uniffi::UniFfiStream<u32, std::convert::Infallible>;
    async fn items(
        &self,
    ) -> impl Stream<Item = Result<u32, std::convert::Infallible>> + SendOutsideWasm + 'static;
    fn factory(
        &self,
    ) -> Result<impl Stream<Item = u32> + SendOutsideWasm + 'static, std::convert::Infallible>;
    async fn async_factory(
        &self,
    ) -> Result<uniffi::UniFfiStream<u32, std::convert::Infallible>, std::convert::Infallible>;
    fn future_factory(
        &self,
    ) -> BoxFuture<
        'static,
        Result<uniffi::UniFfiStream<u32, std::convert::Infallible>, std::convert::Infallible>,
    >;
}
fn certain<T>(value: Result<T, std::convert::Infallible>) -> T {
    match value {
        Ok(value) => value,
        Err(never) => match never {},
    }
}
#[uniffi::export(callback(
    path = "argument[0]",
    retention = "retained",
    threading = "may_cross_thread",
    reentrancy = "allowed"
))]
pub async fn host_infallible(provider: Arc<dyn InfallibleProvider>, kind: u32) -> Vec<u32> {
    match kind {
        8 => {
            certain(provider.unit());
            vec![99]
        }
        9 => {
            certain(provider.async_unit().await);
            vec![100]
        }
        10 => {
            let future = provider.future_unit();
            drop(provider);
            certain(future.await);
            vec![101]
        }
        0 => vec![certain(provider.value())],
        1 => vec![certain(provider.async_value().await)],
        2 => {
            let future = provider.future_value();
            drop(provider);
            vec![certain(future.await)]
        }
        3 => provider.canonical().map(certain).collect().await,
        4 => provider.items().await.map(certain).collect().await,
        5 => certain(provider.factory()).collect().await,
        6 => {
            certain(provider.async_factory().await)
                .map(certain)
                .collect()
                .await
        }
        _ => {
            let future = provider.future_factory();
            drop(provider);
            certain(future.await).map(certain).collect().await
        }
    }
}

// R1-02: the original foreign producer proxy moves into the subscription.
#[uniffi::export]
impl StreamService {
    pub fn echo(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> uniffi::UniFfiStream<u32, FixtureError> {
        Box::pin(input)
    }
    pub async fn echo_async(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> uniffi::UniFfiStream<u32, FixtureError> {
        Box::pin(input)
    }
    pub async fn echo_fail(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> Result<uniffi::UniFfiStream<u32, FixtureError>, FixtureError> {
        drop(input);
        Err(FixtureError::Factory)
    }
    pub async fn echo_cancel(
        &self,
        input: UniFfiInputStream<u32, FixtureError>,
    ) -> uniffi::UniFfiStream<u32, FixtureError> {
        uniffi::async_runtime::current_context().unwrap().cancel();
        Box::pin(input)
    }
}

#[uniffi::export]
pub fn infallible_unit() -> Result<(), std::convert::Infallible> {
    Ok(())
}
#[uniffi::export]
pub async fn infallible_async_unit() -> Result<(), std::convert::Infallible> {
    Ok(())
}
#[uniffi::export]
pub fn infallible_future_unit() -> BoxFuture<'static, Result<(), std::convert::Infallible>> {
    Box::pin(async { Ok(()) })
}
#[derive(uniffi::Object)]
pub struct ResultConstructed {
    value: u32,
}
#[uniffi::export]
impl ResultConstructed {
    #[uniffi::constructor]
    pub fn new(value: u32) -> Result<Self, std::convert::Infallible> {
        Ok(Self { value })
    }
    #[uniffi::constructor]
    pub async fn new_arc(value: u32) -> Result<Arc<Self>, std::convert::Infallible> {
        Ok(Arc::new(Self { value }))
    }
    #[uniffi::constructor]
    pub fn new_future(
        value: u32,
    ) -> BoxFuture<'static, Result<Arc<Self>, std::convert::Infallible>> {
        Box::pin(async move { Ok(Arc::new(Self { value })) })
    }
    pub fn value(&self) -> u32 {
        self.value
    }
}
