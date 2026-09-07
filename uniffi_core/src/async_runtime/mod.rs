//! Opt-in task, cancellation and time support shared by native and JavaScript WASM.
//!
//! Native [`spawn`] futures must be `Send`, but never need to be `Sync`.
//! [`LocalExecutor`] constructs and runs !Send futures on a fixed owner. WASM
//! futures stay in their JavaScript instance. Native spawn and timers use Tokio.
//! Ordinary [`CallContext::run`] retains auto traits and needs no executor.
//!
//! Cancellation drops the business future before polling explicitly registered
//! cleanup. Dropping a `Run` also drops its business future synchronously; retain
//! the context and await [`CallContext::wait_closed`] to drive remaining cleanup.
//! Arbitrary Rust destructors cannot perform asynchronous cleanup automatically.

pub mod actor;
mod context;
mod local;
mod local_stream;
mod task;
mod time;

pub use context::{
    current_context, CallContext, CancellationToken, Cancelled, CleanupClosed, CleanupFailed, Run,
};
pub use local::{
    defer_local_cleanup, spawn_local, LocalBoxFuture, LocalError, LocalExecutor, LocalObject,
};
pub use local_stream::local_stream;
pub use task::{spawn, TaskError, TaskHandle};
pub use time::{sleep, timeout, Elapsed};

use std::{future::Future, pin::Pin};

/// A bound which requires `Send` on native and allows instance-local WASM values.
#[cfg(not(target_arch = "wasm32"))]
pub trait SendOutsideWasm: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> SendOutsideWasm for T {}
#[cfg(target_arch = "wasm32")]
pub trait SendOutsideWasm {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> SendOutsideWasm for T {}

/// A bound for shared objects, separate from the bounds of futures and streams.
#[cfg(not(target_arch = "wasm32"))]
pub trait SyncOutsideWasm: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> SyncOutsideWasm for T {}
#[cfg(target_arch = "wasm32")]
pub trait SyncOutsideWasm {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> SyncOutsideWasm for T {}

#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
#[cfg(not(target_arch = "wasm32"))]
pub type BoxStream<'a, T> = Pin<Box<dyn futures_core::Stream<Item = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type BoxStream<'a, T> = Pin<Box<dyn futures_core::Stream<Item = T> + 'a>>;

/// Run a call with a fresh cancellation context.
pub fn run<F: Future>(future: F) -> Run<F> {
    CallContext::new(CancellationToken::new()).run(future)
}

/// Native tasks cannot capture instance-local values across an await.
///
/// ```compile_fail
/// # #[cfg(not(target_arch = "wasm32"))] {
/// use std::rc::Rc;
/// let local = Rc::new(1);
/// uniffi_core::async_runtime::spawn(async move {
///     std::future::pending::<()>().await;
///     drop(local);
/// });
/// # }
/// ```
///
/// Task outputs must also be transferable even if the future itself is Send.
///
/// ```compile_fail
/// # #[cfg(not(target_arch = "wasm32"))] {
/// uniffi_core::async_runtime::spawn(async { std::rc::Rc::new(1) });
/// # }
/// ```
///
/// Cleanup also needs to be transferable on native.
///
/// ```compile_fail
/// # #[cfg(not(target_arch = "wasm32"))] {
/// use uniffi_core::async_runtime::{CallContext, CancellationToken};
/// let context = CallContext::new(CancellationToken::new());
/// let local = std::rc::Rc::new(1);
/// context.defer_cleanup(async move { std::future::pending::<()>().await; drop(local); });
/// # }
/// ```
#[cfg(not(target_arch = "wasm32"))]
const _: () = ();
