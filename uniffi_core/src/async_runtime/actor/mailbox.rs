use super::*;
use futures_util::{
    future::{poll_fn, Either},
    FutureExt,
};
use parking_lot::Mutex;
use std::{
    future::{ready, Ready},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    task::{Context, Poll},
};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, TryAcquireError};

type ReplyFuture =
    Either<Ready<Result<(), ActorError>>, BoxFuture<'static, Result<(), ActorError>>>;
#[cfg(not(target_arch = "wasm32"))]
type ReplyDelivery = Box<dyn FnOnce(bool) -> ReplyFuture + Send>;
#[cfg(target_arch = "wasm32")]
type ReplyDelivery = Box<dyn FnOnce(bool) -> ReplyFuture>;

// Completion does not retain the Request or its reply. The producer must not
// move destruction of business state past the existing acknowledgement point.
pub(super) struct RequestCompletion {
    result: Mutex<Option<Result<(), ActorError>>>,
    changed: Notify,
}
fn request_completion() -> (RequestFinished, Arc<RequestCompletion>) {
    let signal = Arc::new(RequestCompletion {
        result: Mutex::new(None),
        changed: Notify::new(),
    });
    (RequestFinished(Some(signal.clone())), signal)
}
impl RequestCompletion {
    fn publish(&self, result: Result<(), ActorError>) {
        *self.result.lock() = Some(result);
        self.changed.notify_waiters();
    }
    async fn wait(&self) -> Result<(), ActorError> {
        loop {
            // notify_waiters is remembered from Notified creation, including
            // a completion between this result check and the first poll.
            let notified = self.changed.notified();
            if let Some(result) = *self.result.lock() {
                return result;
            }
            notified.await;
        }
    }
    fn defer(self: Arc<Self>) {
        if let Some(context) = super::super::current_context() {
            context
                .defer_cleanup(async move {
                    match self.wait().await {
                        Ok(()) | Err(ActorError::Cancelled) => {}
                        Err(error) => panic!("actor cleanup failed: {error}"),
                    }
                })
                .expect("current context accepts actor completion");
        }
    }
}
pub(super) struct RequestFinished(Option<Arc<RequestCompletion>>);
impl RequestFinished {
    pub(super) fn finish(mut self, result: Result<(), ActorError>) {
        if let Some(signal) = self.0.take() {
            signal.publish(result);
        }
    }
}
impl Drop for RequestFinished {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            signal.publish(Err(ActorError::ExecutorStopped));
        }
    }
}

pub(super) struct Request {
    token: OnceLock<CancellationToken>,
    pub accepted: AtomicBool,
    pub signal: Arc<RequestCompletion>,
    pub control: Arc<Control>,
    // Retain output on the execution owner until handler cleanup and terminal
    // arbitration finish. In particular, an error must not leave a resource in
    // the caller's oneshot to be dropped after the completion signal.
    pub reply: Mutex<Option<ReplyDelivery>>,
}
impl Request {
    pub fn token(&self) -> &CancellationToken {
        self.token.get_or_init(CancellationToken::new)
    }
    pub fn is_cancelled(&self) -> bool {
        self.token
            .get()
            .is_some_and(CancellationToken::is_cancelled)
    }
    pub fn cancel(&self) {
        self.token().cancel();
        self.control.changed.notify_one();
    }
    pub fn hold_reply<R: SendOutsideWasm + 'static>(
        &self,
        output: R,
        sender: oneshot::Sender<R>,
        local: bool,
    ) {
        *self.reply.lock() = Some(Box::new(move |deliver| -> ReplyFuture {
            // The driver invokes this closure inside the reply CallContext.
            let (undelivered, sender) = if deliver {
                (sender.send(output).err(), None)
            } else {
                (Some(output), Some(sender))
            };
            if let Some(output) = undelivered {
                if local {
                    return Either::Right(Box::pin(async move {
                        // Keep a cancelled delivery's sender until cleanup ends.
                        let _sender = sender;
                        // A fresh S02 root confirms last-proxy finalizers for
                        // this reply, independently of the actor's own root.
                        let task = super::super::spawn_local(async move { drop(output) })
                            .map_err(ActorError::from)?;
                        task.wait().await.map_err(ActorError::from)?;
                        Ok(())
                    }));
                }
                drop(output);
            }
            Either::Left(ready(Ok(())))
        }));
    }
}
pub(super) struct Packet<E> {
    pub envelope: E,
    pub request: Arc<Request>,
    pub finished: RequestFinished,
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
            self.request.signal.clone().defer();
        }
    }
}
/// Immediate admission failure, retaining the undelivered message.
#[derive(Debug)]
pub enum TrySendError<M> {
    Full(M),
    Closed(M),
}

impl<A: Actor, E: Payload> Clone for ActorRef<A, E> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            control: self.control.clone(),
            marker: PhantomData,
        }
    }
}
impl<A: Actor, E: Payload> ActorRef<A, E> {
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
        let (finished, signal) = request_completion();
        let request = Arc::new(Request {
            token: OnceLock::new(),
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
            job.signal.wait().await?;
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
        let (finished, signal) = request_completion();
        let request = Arc::new(Request {
            token: OnceLock::new(),
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
        let state = self.control.state.lock();
        if self.control.status().mode != Mode::Running || self.sender.is_closed() {
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
        let (finished, signal) = request_completion();
        let request = Arc::new(Request {
            token: OnceLock::new(),
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
    let request = packet.request.clone();
    let mut cancelled = std::pin::pin!(async { request.token().cancelled().await });
    let mut task_cancelled = std::pin::pin!(async {
        match &task_token {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    });
    let mut stopped = std::pin::pin!(stop.cancelled());
    let mut acquire = std::pin::pin!(control.slots.clone().acquire_owned());
    let permit = poll_fn(|cx| {
        if stop.is_cancelled() {
            return Poll::Ready(Err(ActorError::Closed));
        }
        if request.is_cancelled()
            || task_token
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Poll::Ready(Err(ActorError::Cancelled));
        }
        // Keep the same acquire future for FIFO and cooperative budget checks.
        // A Ready admission needs no cancellation waiter registration.
        if let Poll::Ready(result) = acquire.as_mut().poll(cx) {
            return Poll::Ready(result.map_err(|_| ActorError::Closed));
        }
        // Register before returning Pending, rechecking cancellation that may
        // have arrived during the acquire poll.
        if stopped.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ActorError::Closed));
        }
        if cancelled.as_mut().poll(cx).is_ready() || task_cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ActorError::Cancelled));
        }
        Poll::Pending
    })
    .await;
    let admission: Result<(), ActorError> = match permit {
        Ok(permit) => {
            let _state = control.state.lock();
            let status = control.status();
            if status.mode == Mode::Closing || (!necessary && status.mode != Mode::Running) {
                Err(ActorError::Closed)
            } else if request.is_cancelled()
                || task_token
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            {
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

#[cfg(test)]
mod request_completion_tests {
    use super::*;
    use futures_util::task::{waker, ArcWake};
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Wakes(AtomicUsize);
    impl ArcWake for Wakes {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancelled_or_dropped_waiter_preserves_other_and_late_waiters() {
        for cancel in [false, true] {
            let (finished, signal) = request_completion();
            let context = CallContext::new(CancellationToken::new());
            let abandoned_wakes = Arc::new(Wakes::default());
            let left_wakes = Arc::new(Wakes::default());
            let right_wakes = Arc::new(Wakes::default());
            let abandoned_waker = waker(abandoned_wakes.clone());
            let left_waker = waker(left_wakes.clone());
            let right_waker = waker(right_wakes.clone());
            let mut abandoned_cx = Context::from_waker(&abandoned_waker);
            let mut left_cx = Context::from_waker(&left_waker);
            let mut right_cx = Context::from_waker(&right_waker);
            let mut abandoned = Box::pin(context.run(signal.wait()));
            let mut left = std::pin::pin!(signal.wait());
            let mut right = std::pin::pin!(signal.wait());
            assert!(abandoned.as_mut().poll(&mut abandoned_cx).is_pending());
            assert!(left.as_mut().poll(&mut left_cx).is_pending());
            assert!(right.as_mut().poll(&mut right_cx).is_pending());
            if cancel {
                context.cancel();
                assert!(matches!(
                    abandoned.as_mut().poll(&mut abandoned_cx),
                    Poll::Ready(Err(_))
                ));
            }
            drop(abandoned);
            abandoned_wakes.0.store(0, Ordering::SeqCst);
            assert!(left.as_mut().poll(&mut left_cx).is_pending());
            assert!(right.as_mut().poll(&mut right_cx).is_pending());

            finished.finish(Ok(()));
            assert_eq!(abandoned_wakes.0.load(Ordering::SeqCst), 0);
            assert!(left_wakes.0.load(Ordering::SeqCst) > 0);
            assert!(right_wakes.0.load(Ordering::SeqCst) > 0);
            assert_eq!(left.as_mut().poll(&mut left_cx), Poll::Ready(Ok(())));
            assert_eq!(right.as_mut().poll(&mut right_cx), Poll::Ready(Ok(())));
            let mut late = std::pin::pin!(signal.wait());
            assert_eq!(late.as_mut().poll(&mut left_cx), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn producer_loss_wakes_waiters_and_preserves_executor_stopped() {
        let (finished, signal) = request_completion();
        let wakes = Arc::new(Wakes::default());
        let waker = waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut waiting = std::pin::pin!(signal.wait());
        let mut unpolled = std::pin::pin!(signal.wait());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        drop(finished);
        assert!(wakes.0.load(Ordering::SeqCst) > 0);
        assert_eq!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Err(ActorError::ExecutorStopped))
        );
        assert_eq!(
            unpolled.as_mut().poll(&mut cx),
            Poll::Ready(Err(ActorError::ExecutorStopped))
        );
        let mut late = std::pin::pin!(signal.wait());
        assert_eq!(
            late.as_mut().poll(&mut cx),
            Poll::Ready(Err(ActorError::ExecutorStopped))
        );
    }
}
