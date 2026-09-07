//! Typed, bounded actors with explicit lifecycle ownership.
//!
//! A handler borrows the actor exclusively until it completes, including across
//! awaits. Request cancellation is cooperative: observe the context token and
//! finish restoring state before returning. Never await this actor's next message
//! from a handler; use a managed task and a completion message instead.

use super::{
    BoxFuture, CallContext, CancellationToken, LocalBoxFuture, LocalExecutor, SendOutsideWasm,
};
use std::sync::Arc;
use tokio::sync::mpsc;
mod control;
mod driver;
mod mailbox;
mod managed;
use control::*;
pub use driver::{start, start_local, ActorOwner};
use mailbox::{discard, Packet};
pub use mailbox::{Ask, TrySendError};
pub use managed::{ActorContext, ManagedContext};

/// Framework failures are separate from the business value in Message::Reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorError {
    InvalidCapacity,
    Closed,
    Cancelled,
    Panicked,
    ExecutorStopped,
    StartFailed,
}
impl std::fmt::Display for ActorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "actor {self:?}")
    }
}
impl std::error::Error for ActorError {}
impl From<super::LocalError> for ActorError {
    fn from(error: super::LocalError) -> Self {
        match error {
            super::LocalError::InvalidCapacity => Self::InvalidCapacity,
            super::LocalError::Closed => Self::Closed,
            super::LocalError::Cancelled => Self::Cancelled,
            super::LocalError::Panicked => Self::Panicked,
            super::LocalError::StartFailed | super::LocalError::NoOwner => Self::StartFailed,
            super::LocalError::ExecutorStopped => Self::ExecutorStopped,
        }
    }
}
impl From<super::TaskError> for ActorError {
    fn from(error: super::TaskError) -> Self {
        match error {
            super::TaskError::Cancelled => Self::Cancelled,
            super::TaskError::Panicked => Self::Panicked,
            super::TaskError::ExecutorStopped => Self::ExecutorStopped,
        }
    }
}
use futures_channel::oneshot;
use std::{future::Future, marker::PhantomData};

/// Actor state. Send/Sync requirements belong to execution entry points.
pub trait Actor: 'static {}
/// A typed message protocol; business errors belong in `Reply` (usually Result).
pub trait Message: SendOutsideWasm + 'static {
    type Reply: SendOutsideWasm + 'static;
}
/// Implement once for either executor. A boxed Send or local future is often
/// the simplest GAT implementation; Send actors never need to be Sync.
pub trait Handler<M: Message>: Actor {
    type Future<'a>: Future<Output = M::Reply> + 'a
    where
        Self: 'a;
    fn handle<'a>(&'a mut self, message: M, context: ActorContext) -> Self::Future<'a>;
}

#[doc(hidden)]
pub trait Payload: SendOutsideWasm + 'static {
    fn release(self) -> BoxFuture<'static, Result<(), ActorError>>;
}
#[doc(hidden)]
pub trait Envelope<A: Actor>: Payload {
    type Future<'a>: Future<Output = Result<(), ActorError>> + 'a
    where
        A: 'a;
    fn handle<'a>(self, actor: &'a mut A, context: ActorContext) -> Self::Future<'a>;
}
#[doc(hidden)]
pub trait Wrap<A: Actor, M: Message>: Envelope<A> {
    fn wrap(message: M, reply: oneshot::Sender<M::Reply>) -> Self;
}
#[cfg(not(target_arch = "wasm32"))]
type SendAction<A> = Box<
    dyn for<'a> FnOnce(&'a mut A, ActorContext) -> BoxFuture<'a, Result<(), ActorError>> + Send,
>;
#[cfg(target_arch = "wasm32")]
type SendAction<A> =
    Box<dyn for<'a> FnOnce(&'a mut A, ActorContext) -> BoxFuture<'a, Result<(), ActorError>>>;
#[cfg(not(target_arch = "wasm32"))]
type LocalAction<A> = Box<
    dyn for<'a> FnOnce(&'a mut A, ActorContext) -> LocalBoxFuture<'a, Result<(), ActorError>>
        + Send,
>;
#[cfg(target_arch = "wasm32")]
type LocalAction<A> =
    Box<dyn for<'a> FnOnce(&'a mut A, ActorContext) -> LocalBoxFuture<'a, Result<(), ActorError>>>;
#[doc(hidden)]
pub struct SendEnvelope<A: Actor>(SendAction<A>);
#[doc(hidden)]
pub struct LocalEnvelope<A: Actor>(LocalAction<A>);
impl<A: Actor> Payload for SendEnvelope<A> {
    fn release(self) -> BoxFuture<'static, Result<(), ActorError>> {
        Box::pin(async move {
            drop(self);
            Ok(())
        })
    }
}
impl<A: Actor> Payload for LocalEnvelope<A> {
    fn release(self) -> BoxFuture<'static, Result<(), ActorError>> {
        Box::pin(async move {
            let task = super::spawn_local(async move {
                drop(self);
            })
            .map_err(ActorError::from)?;
            task.wait().await.map_err(ActorError::from)
        })
    }
}
impl<A: Actor> Envelope<A> for SendEnvelope<A> {
    type Future<'a> = BoxFuture<'a, Result<(), ActorError>>;
    fn handle<'a>(self, actor: &'a mut A, context: ActorContext) -> Self::Future<'a> {
        (self.0)(actor, context)
    }
}
impl<A: Actor> Envelope<A> for LocalEnvelope<A> {
    type Future<'a> = LocalBoxFuture<'a, Result<(), ActorError>>;
    fn handle<'a>(self, actor: &'a mut A, context: ActorContext) -> Self::Future<'a> {
        (self.0)(actor, context)
    }
}
impl<A, M> Wrap<A, M> for SendEnvelope<A>
where
    A: Handler<M> + SendOutsideWasm,
    M: Message,
    for<'a> A::Future<'a>: SendOutsideWasm,
{
    fn wrap(message: M, reply: oneshot::Sender<M::Reply>) -> Self {
        Self(Box::new(move |actor, context| {
            Box::pin(async move {
                let request = context.request.clone();
                let output = actor.handle(message, context).await;
                request.hold_reply(output, reply, false);
                Ok(())
            })
        }))
    }
}
impl<A: Handler<M>, M: Message> Wrap<A, M> for LocalEnvelope<A> {
    fn wrap(message: M, reply: oneshot::Sender<M::Reply>) -> Self {
        Self(Box::new(move |actor, context| {
            Box::pin(async move {
                let request = context.request.clone();
                let output = actor.handle(message, context).await;
                request.hold_reply(output, reply, true);
                Ok(())
            })
        }))
    }
}

/// A transferable address; it contains neither A nor a reference to A.
pub struct ActorRef<A: Actor, E = SendEnvelope<A>> {
    sender: mpsc::Sender<Packet<E>>,
    control: Arc<Control>,
    marker: PhantomData<fn() -> (A, E)>,
}
pub type LocalActorRef<A> = ActorRef<A, LocalEnvelope<A>>;

/// Native local state cannot be passed to the Send entry point.
/// ```compile_fail
/// use std::rc::Rc;
/// use uniffi_core::async_runtime::actor::{Actor, start};
/// struct Local(Rc<()>);
/// impl Actor for Local {}
/// let _ = start(Local(Rc::new(())), 1);
/// ```
/// A local factory cannot transfer a preexisting Rc to another native thread.
/// ```compile_fail
/// use std::rc::Rc;
/// use uniffi_core::async_runtime::{actor::{Actor, start_local}, LocalExecutor};
/// struct Local(Rc<()>);
/// impl Actor for Local {}
/// async fn bad(executor: LocalExecutor) {
///     let rc = Rc::new(());
///     let _ = start_local(&executor, move || Local(rc), 1).await;
/// }
/// ```
/// Native local replies cannot smuggle owner-local state across threads.
/// ```compile_fail
/// use std::rc::Rc;
/// use uniffi_core::async_runtime::actor::Message;
/// struct Escape;
/// impl Message for Escape { type Reply = Rc<()>; }
/// ```
/// A Send actor may not use a !Send handler future through its Send address.
/// ```compile_fail
/// use uniffi_core::async_runtime::{LocalBoxFuture, actor::*};
/// struct A; impl Actor for A {}
/// struct M; impl Message for M { type Reply = (); }
/// impl Handler<M> for A {
///     type Future<'a> = LocalBoxFuture<'a, ()>;
///     fn handle<'a>(&'a mut self, _: M, _: ActorContext) -> Self::Future<'a> {
///         Box::pin(async {})
///     }
/// }
/// fn bad(address: ActorRef<A>) { let _ = address.ask(M); }
/// ```
#[cfg(not(target_arch = "wasm32"))]
const _: () = ();
