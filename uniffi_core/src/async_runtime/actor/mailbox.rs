use super::*;
use futures_util::{future::poll_fn, FutureExt};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, TryAcquireError};

#[cfg(not(target_arch = "wasm32"))]
type ReplyDelivery = Box<dyn FnOnce(bool) -> BoxFuture<'static, Result<(), ActorError>> + Send>;
#[cfg(target_arch = "wasm32")]
type ReplyDelivery = Box<dyn FnOnce(bool) -> BoxFuture<'static, Result<(), ActorError>>>;

pub(super) struct Request {
    pub token: CancellationToken,
    pub accepted: AtomicBool,
    pub signal: Signal,
    pub control: Arc<Control>,
    // Retain output on the execution owner until handler cleanup and terminal
    // arbitration finish. In particular, an error must not leave a resource in
    // the caller's oneshot to be dropped after the completion signal.
    pub reply: Mutex<Option<ReplyDelivery>>,
}
impl Request {
    pub fn cancel(&self) {
        self.token.cancel();
        self.control.changed.notify_one();
    }
    pub fn hold_reply<R: SendOutsideWasm + 'static>(
        &self,
        output: R,
        sender: oneshot::Sender<R>,
        local: bool,
    ) {
        *self.reply.lock().unwrap() = Some(Box::new(move |deliver| {
            Box::pin(async move {
                let undelivered = if deliver {
                    sender.send(output).err()
                } else {
                    Some(output)
                };
                if let Some(output) = undelivered {
                    if local {
                        // A fresh S02 root confirms last-proxy finalizers for
                        // this reply, independently of the actor's own root.
                        let task = super::super::spawn_local(async move { drop(output) })
                            .map_err(ActorError::from)?;
                        task.wait().await.map_err(ActorError::from)?;
                    } else {
                        drop(output);
                    }
                }
                Ok(())
            })
        }));
    }
}
pub(super) struct Packet<E> {
    pub envelope: E,
    pub request: Arc<Request>,
    pub finished: Finished,
    pub permit: Option<OwnedSemaphorePermit>,
}
/// A cancellable typed reply future. Drop cancels accepted work and transfers
/// its owner-cleanup confirmation into the current S01 CallContext, when any.
#[must_use = "ask is lazy; await it or explicitly cancel_and_wait"]
pub struct Ask<R> {
    future: Option<BoxFuture<'static, Result<R, ActorError>>>,
    request: Arc<Request>,
    completed: bool,
}
impl<R> Unpin for Ask<R> {}
impl<R> Ask<R> {
    pub fn cancel(&self) {
        self.request.cancel();
    }
    pub async fn cancel_and_wait(self) -> Result<R, ActorError> {
        self.cancel();
        self.await
    }
}
impl<R> Future for Ask<R> {
    type Output = Result<R, ActorError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self
            .future
            .as_mut()
            .expect("ask polled after completion")
            .as_mut()
            .poll(cx);
        if result.is_ready() {
            self.completed = true;
            self.future = None;
        }
        result
    }
}
impl<R> Drop for Ask<R> {
    fn drop(&mut self) {
        if !self.completed {
            self.request.cancel();
        }
        drop(self.future.take());
        if !self.completed && self.request.accepted.load(Ordering::Acquire) {
            defer(self.request.signal.clone());
        }
    }
}
/// Immediate admission failure, retaining the undelivered message.
#[derive(Debug)]
pub enum TrySendError<M> {
    Full(M),
    Closed(M),
}

impl<A: Actor, E: Envelope<A>> Clone for ActorRef<A, E> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            control: self.control.clone(),
            marker: PhantomData,
        }
    }
}
impl<A: Actor, E: Envelope<A>> ActorRef<A, E> {
    pub fn is_closed(&self) -> bool {
        self.control.stopping.is_cancelled() || self.sender.is_closed()
    }
    pub fn managed_count(&self) -> usize {
        self.control.managed_count()
    }
    pub fn ask<M: Message>(&self, message: M) -> Ask<M::Reply>
    where
        E: Wrap<A, M>,
    {
        let (finished, signal) = completion();
        let request = Arc::new(Request {
            token: CancellationToken::new(),
            accepted: AtomicBool::new(false),
            signal,
            control: self.control.clone(),
            reply: Mutex::new(None),
        });
        let (reply, receiver) = oneshot::channel();
        let packet = Packet {
            envelope: E::wrap(message, reply),
            request: request.clone(),
            finished,
            permit: None,
        };
        let sender = self.sender.clone();
        let job = request.clone();
        let future = Box::pin(async move {
            submit(sender, packet, false, None).await?;
            wait(job.signal.clone()).await?;
            receiver.await.map_err(|_| ActorError::ExecutorStopped)
        });
        Ask {
            future: Some(future),
            request,
            completed: false,
        }
    }
    /// Wait for bounded admission only. Subsequent close can cancel this message.
    pub async fn tell<M: Message>(&self, message: M) -> Result<(), ActorError>
    where
        E: Wrap<A, M>,
    {
        self.deliver(message, false, None).await
    }
    pub async fn send<M: Message>(&self, message: M) -> Result<(), ActorError>
    where
        E: Wrap<A, M>,
    {
        self.tell(message).await
    }
    pub(super) async fn deliver<M: Message>(
        &self,
        message: M,
        necessary: bool,
        token: Option<CancellationToken>,
    ) -> Result<(), ActorError>
    where
        E: Wrap<A, M>,
    {
        let (finished, signal) = completion();
        let request = Arc::new(Request {
            token: CancellationToken::new(),
            accepted: AtomicBool::new(false),
            signal,
            control: self.control.clone(),
            reply: Mutex::new(None),
        });
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        submit(
            self.sender.clone(),
            Packet {
                envelope: E::wrap(message, reply),
                request,
                finished,
                permit: None,
            },
            necessary,
            token,
        )
        .await
    }
    pub fn try_send<M: Message>(&self, message: M) -> Result<(), TrySendError<M>>
    where
        E: Wrap<A, M>,
    {
        let state = self.control.state.lock().unwrap();
        if state.mode != Mode::Running || self.sender.is_closed() {
            return Err(TrySendError::Closed(message));
        }
        let permit = match self.control.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => return Err(TrySendError::Full(message)),
            Err(TryAcquireError::Closed) => return Err(TrySendError::Closed(message)),
        };
        let channel_permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(_) => return Err(TrySendError::Closed(message)),
        };
        let (finished, signal) = completion();
        let request = Arc::new(Request {
            token: CancellationToken::new(),
            accepted: AtomicBool::new(true),
            signal,
            control: self.control.clone(),
            reply: Mutex::new(None),
        });
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        // One shared semaphore bounds both the channel and the owner's pending
        // deque. Holding its permit guarantees room in the bounded channel.
        channel_permit.send(Packet {
            envelope: E::wrap(message, reply),
            request,
            finished,
            permit: Some(permit),
        });
        drop(state);
        Ok(())
    }
}
async fn submit<E: Payload>(
    sender: mpsc::Sender<Packet<E>>,
    mut packet: Packet<E>,
    necessary: bool,
    task_token: Option<CancellationToken>,
) -> Result<(), ActorError> {
    let control = packet.request.control.clone();
    let stop = if necessary {
        control.cancelling.clone()
    } else {
        control.stopping.clone()
    };
    let token = packet.request.token.clone();
    let task_token = task_token.unwrap_or_default();
    let mut cancelled = Box::pin(token.cancelled());
    let mut task_cancelled = Box::pin(task_token.cancelled());
    let mut stopped = Box::pin(stop.cancelled());
    let mut acquire = Box::pin(control.slots.clone().acquire_owned());
    let permit = poll_fn(|cx| {
        if stopped.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ActorError::Closed));
        }
        if cancelled.as_mut().poll(cx).is_ready() || task_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ActorError::Cancelled));
        }
        acquire
            .as_mut()
            .poll(cx)
            .map(|r| r.map_err(|_| ActorError::Closed))
    })
    .await;
    let admission: Result<(), ActorError> = match permit {
        Ok(permit) => {
            let state = control.state.lock().unwrap();
            if state.mode == Mode::Closing || (!necessary && state.mode != Mode::Running) {
                Err(ActorError::Closed)
            } else if token.is_cancelled() || task_token.is_cancelled() {
                Err(ActorError::Cancelled)
            } else {
                packet.permit = Some(permit);
                packet.request.accepted.store(true, Ordering::Release);
                match sender.try_send(packet) {
                    Ok(()) => return Ok(()),
                    Err(
                        mpsc::error::TrySendError::Closed(value)
                        | mpsc::error::TrySendError::Full(value),
                    ) => {
                        packet = value;
                        Err(ActorError::Closed)
                    }
                }
            }
        }
        Err(error) => Err(error),
    };
    let error = admission.unwrap_err();
    discard(packet, error, false).await?;
    Err(error)
}
/// Drop queued payloads inside a fresh cleanup scope on their execution owner.
/// Permit stays held until async release completes, bounding cleanup backlog too.
pub(super) async fn discard<E: Payload>(
    packet: Packet<E>,
    error: ActorError,
    on_owner: bool,
) -> Result<(), ActorError> {
    let Packet {
        envelope,
        request: _,
        finished,
        permit,
    } = packet;
    let scope = CallContext::new(CancellationToken::new());
    let mut cleanup_guard = CleanupGuard(Some(scope.clone()));
    let result = std::panic::AssertUnwindSafe(scope.run(async move {
        if on_owner {
            envelope.release().await
        } else {
            drop(envelope);
            Ok(())
        }
    }))
    .catch_unwind()
    .await;
    let cleanup = scope.wait_closed().await;
    cleanup_guard.0 = None;
    drop(permit);
    let result = match (result, cleanup) {
        (Ok(Ok(result)), Ok(())) => result,
        _ => Err(ActorError::Panicked),
    };
    finished.finish(result.and(Err(error)));
    result
}
