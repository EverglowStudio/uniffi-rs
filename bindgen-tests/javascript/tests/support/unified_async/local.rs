//! Same thread-affine business object on native and WASM, through generated bindings.
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};
use uniffi::async_runtime::{defer_local_cleanup, sleep, LocalExecutor, LocalObject};

use crate::{gate, update, FixtureError};

#[derive(Clone)]
struct OwnerThread {
    #[cfg(not(target_arch = "wasm32"))]
    id: std::thread::ThreadId,
}
impl OwnerThread {
    fn here() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            id: std::thread::current().id(),
        }
    }
    fn check(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        assert_eq!(
            self.id,
            std::thread::current().id(),
            "local body escaped owner"
        );
    }
}

#[derive(uniffi::Object)]
pub struct LocalComponent {
    executor: LocalExecutor,
}
#[uniffi::export]
impl LocalComponent {
    #[uniffi::constructor]
    pub fn new(capacity: u32) -> Arc<Self> {
        Arc::new(Self {
            executor: LocalExecutor::new(capacity as usize).expect("valid fixture capacity"),
        })
    }
    pub async fn close(&self) {
        self.executor.close().await.expect("owner cleanup");
    }
    pub async fn cancel_returned_counter(
        self: Arc<Self>,
        id: u32,
    ) -> Arc<LocalObject<LocalCounter>> {
        let counter = LocalCounter::new(self, id, 0).await;
        uniffi::async_runtime::current_context()
            .expect("FFI call context")
            .cancel();
        counter
    }
}

#[derive(uniffi::Object)]
#[uniffi(local, name = "OwnerCounter")]
pub struct LocalCounter {
    id: u32,
    value: Rc<Cell<u32>>,
    owner: OwnerThread,
}

impl Drop for LocalCounter {
    fn drop(&mut self) {
        self.owner.check();
        let id = self.id;
        let value = self.value.clone();
        let owner = self.owner.clone();
        update(id, |s| s.dropped += 1);
        defer_local_cleanup(async move {
            owner.check();
            update(id, |s| s.cleaning += 1);
            gate(id, 8, false).await;
            owner.check();
            value.set(value.get().wrapping_add(1));
            update(id, |s| s.cleaned += 1);
        })
        .expect("object Drop has local cleanup context");
    }
}

#[uniffi::export(local, name = "OwnerCounter")]
impl LocalCounter {
    #[uniffi::constructor]
    pub async fn new(
        component: Arc<LocalComponent>,
        id: u32,
        value: u32,
    ) -> Arc<LocalObject<Self>> {
        Arc::new(
            component
                .executor
                .create(move || async move {
                    let owner = OwnerThread::here();
                    let value = Rc::new(Cell::new(value));
                    sleep(Duration::ZERO).await;
                    owner.check();
                    update(id, |s| s.started += 1);
                    Self { id, value, owner }
                })
                .await
                .expect("fixture owner open"),
        )
    }

    pub fn value(&self) -> u32 {
        self.owner.check();
        self.value.get()
    }

    pub fn add_label(&self, amount: u32, label: &str) -> u32 {
        self.owner.check();
        self.value
            .set(self.value.get() + amount + label.len() as u32);
        self.value.get()
    }

    pub async fn increment(&self, amount: u32) -> u32 {
        let value = self.value.clone();
        self.owner.check();
        sleep(Duration::from_millis(1)).await;
        self.owner.check();
        value.set(value.get() + amount);
        value.get()
    }

    pub async fn gated_increment(&self, call_id: u32, amount: u32) -> u32 {
        self.owner.check();
        update(call_id, |s| s.started += 1);
        gate(call_id, 1, true).await;
        self.owner.check();
        self.value.set(self.value.get() + amount);
        self.value.get()
    }

    pub async fn guarded(&self, call_id: u32) -> u32 {
        let guard = LocalCallGuard {
            id: call_id,
            value: self.value.clone(),
            owner: self.owner.clone(),
        };
        update(call_id, |s| s.started += 1);
        gate(call_id, 1, true).await;
        drop(guard);
        self.value()
    }

    pub fn fallible(&self, fail: bool) -> Result<u32, FixtureError> {
        self.owner.check();
        if fail {
            Err(FixtureError::Boom)
        } else {
            Ok(self.value())
        }
    }

    #[uniffi::method(callback(
        path = "argument[0]",
        retention = "scoped",
        threading = "may_cross_thread",
        reentrancy = "allowed"
    ))]
    pub async fn visit(&self, visitor: Arc<dyn LocalVisitor>) -> u32 {
        self.owner.check();
        let result = visitor.visit().await;
        self.owner.check();
        result + self.value()
    }
}

#[uniffi::export(rust, foreign)]
pub trait LocalVisitor:
    uniffi::async_runtime::SendOutsideWasm + uniffi::async_runtime::SyncOutsideWasm
{
    async fn visit(&self) -> u32;
}

struct LocalCallGuard {
    id: u32,
    value: Rc<Cell<u32>>,
    owner: OwnerThread,
}
impl Drop for LocalCallGuard {
    fn drop(&mut self) {
        self.owner.check();
        let id = self.id;
        let value = self.value.clone();
        let owner = self.owner.clone();
        update(id, |s| s.dropped += 1);
        defer_local_cleanup(async move {
            owner.check();
            update(id, |s| s.cleaning += 1);
            gate(id, 2, false).await;
            drop(NestedLocalCleanup { id, value, owner });
        })
        .expect("local method Drop context");
    }
}
struct NestedLocalCleanup {
    id: u32,
    value: Rc<Cell<u32>>,
    owner: OwnerThread,
}
impl Drop for NestedLocalCleanup {
    fn drop(&mut self) {
        self.owner.check();
        let id = self.id;
        let value = self.value.clone();
        let owner = self.owner.clone();
        update(id, |s| s.nested += 1);
        defer_local_cleanup(async move {
            gate(id, 4, false).await;
            owner.check();
            value.set(value.get() + 1);
            update(id, |s| s.cleaned += 1);
        })
        .expect("nested local cleanup context");
    }
}

struct LocalRawStream {
    guard: LocalCallGuard,
    remaining: u32,
    pending: bool,
}
impl uniffi::deps::futures_core::Stream for LocalRawStream {
    type Item = Option<u32>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.guard.owner.check();
        update(self.guard.id, |s| s.polls += 1);
        if self.pending {
            return std::task::Poll::Pending;
        }
        if self.remaining == 0 {
            return std::task::Poll::Ready(None);
        }
        self.remaining -= 1;
        update(self.guard.id, |s| s.produced += 1);
        std::task::Poll::Ready(Some(if self.remaining == 1 {
            None
        } else {
            Some(self.guard.value.get())
        }))
    }
}
impl LocalCounter {
    fn raw_stream(&self, id: u32, pending: bool) -> LocalRawStream {
        self.owner.check();
        update(id, |s| s.started += 1);
        LocalRawStream {
            guard: LocalCallGuard {
                id,
                value: self.value.clone(),
                owner: self.owner.clone(),
            },
            remaining: 2,
            pending,
        }
    }
}
#[uniffi::export(local, name = "OwnerCounter")]
impl LocalCounter {
    pub fn raw_values(
        &self,
        id: u32,
        pending: bool,
    ) -> impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static {
        self.raw_stream(id, pending)
    }
    pub async fn cancel_raw_values(
        &self,
        id: u32,
    ) -> impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static {
        let stream = self.raw_stream(id, false);
        uniffi::async_runtime::current_context()
            .expect("local factory context")
            .cancel();
        stream
    }
    pub async fn async_raw_values(
        &self,
        id: u32,
    ) -> impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static {
        self.raw_stream(id, false)
    }
    pub fn future_raw_values(
        &self,
        id: u32,
    ) -> impl std::future::Future<
        Output = impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static,
    > + '_ {
        async move { self.raw_stream(id, false) }
    }
}
trait LocalRawBusiness {
    fn trait_raw_values(
        &self,
        id: u32,
    ) -> impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static;
}
#[uniffi::export(local, name = "OwnerCounter")]
impl LocalRawBusiness for LocalCounter {
    fn trait_raw_values(
        &self,
        id: u32,
    ) -> impl uniffi::deps::futures_core::Stream<Item = Option<u32>> + 'static {
        self.raw_stream(id, false)
    }
}
