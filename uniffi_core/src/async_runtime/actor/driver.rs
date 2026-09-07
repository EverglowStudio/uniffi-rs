use super::*;
use futures_util::{future::poll_fn, stream::FuturesUnordered, FutureExt, Stream};
use std::{
    collections::VecDeque,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::{mpsc, Notify, Semaphore};

/// Explicit lifecycle owner, separate from cloneable addresses. Last owner Drop
/// initiates close; use close/drain_and_close to observe successful release.
#[derive(Clone)]
pub struct ActorOwner {
    inner: Arc<OwnerInner>,
}
struct OwnerInner {
    control: Arc<Control>,
    _executor: Option<LocalExecutor>,
}
impl Drop for OwnerInner {
    fn drop(&mut self) {
        if self.control.stop(Mode::Closing) {
            defer(self.control.done.clone());
        }
    }
}
impl ActorOwner {
    pub fn cancel(&self) {
        self.inner.control.stop(Mode::Closing);
    }
    pub async fn close(&self) -> Result<(), ActorError> {
        self.cancel();
        wait_owned(self.inner.control.done.clone()).await
    }
    pub async fn drain_and_close(&self) -> Result<(), ActorError> {
        self.inner.control.stop(Mode::Draining);
        wait_owned(self.inner.control.done.clone()).await
    }
    pub async fn wait(&self) -> Result<(), ActorError> {
        wait_owned(self.inner.control.done.clone()).await
    }
    pub fn managed_count(&self) -> usize {
        self.inner.control.managed_count()
    }
}
type MailboxSetup<A, E> = (ActorOwner, ActorRef<A, E>, Driver<E>, Finished);
fn mailbox<A: Actor, E: Envelope<A>>(
    capacity: usize,
    executor: Option<LocalExecutor>,
) -> Result<MailboxSetup<A, E>, ActorError> {
    if capacity == 0 || capacity > Semaphore::MAX_PERMITS {
        return Err(ActorError::InvalidCapacity);
    }
    let (finished, done) = completion();
    let control = Arc::new(Control {
        state: Mutex::new(State {
            mode: Mode::Running,
            launched: executor.is_none(),
            failure: None,
            managed: Vec::new(),
        }),
        changed: Notify::new(),
        slots: Arc::new(Semaphore::new(capacity)),
        stopping: CancellationToken::new(),
        cancelling: CancellationToken::new(),
        done,
    });
    let (sender, receiver) = mpsc::channel(capacity);
    Ok((
        ActorOwner {
            inner: Arc::new(OwnerInner {
                control: control.clone(),
                _executor: executor,
            }),
        },
        ActorRef {
            sender,
            control: control.clone(),
            marker: PhantomData,
        },
        Driver {
            receiver,
            queue: VecDeque::new(),
            cleanup: FuturesUnordered::new(),
            control,
        },
        finished,
    ))
}
/// Run Send state on the current native runtime (WASM: current JS instance).
/// The state and handler futures need Send on native; Sync is never required.
pub fn start<A: Actor + SendOutsideWasm>(
    actor: A,
    capacity: usize,
) -> Result<(ActorOwner, ActorRef<A>), ActorError> {
    let (owner, address, driver, finished) = mailbox(capacity, None)?;
    drop(super::super::spawn(async move {
        finished.finish(run(actor, driver).await);
    }));
    Ok((owner, address))
}
/// Construct !Send state on an existing S02 fixed owner. The factory alone
/// crosses threads. Keep the executor running until the actor is closed.
pub async fn start_local<A, F>(
    executor: &LocalExecutor,
    factory: F,
    capacity: usize,
) -> Result<(ActorOwner, LocalActorRef<A>), ActorError>
where
    A: Actor,
    F: FnOnce() -> A + SendOutsideWasm + 'static,
{
    let (owner, address, driver, finished) = mailbox(capacity, Some(executor.clone()))?;
    let control = owner.inner.control.clone();
    let keepalive = executor.clone();
    // The admission task starts an independent observer in one synchronous
    // poll. Cancelling startup after admission cannot cancel that observer or
    // discard the actor's release acknowledgement.
    let admission = executor
        .spawn(move || async move {
            {
                let mut state = control.state.lock().unwrap();
                if state.mode == Mode::Closing {
                    return Err(ActorError::Cancelled);
                }
                state.launched = true;
            }
            let observer = super::super::spawn_local(async move {
                // Last ActorOwner Drop initiates cooperative shutdown; retain
                // the S02 executor until the actor task confirms its releases.
                let _keepalive = keepalive;
                let task = super::super::spawn_local(async move {
                    run(
                        LocalState(std::rc::Rc::new(std::cell::RefCell::new(Some(factory())))),
                        driver,
                    )
                    .await
                });
                let result = match task {
                    Ok(task) => task.wait().await.map_err(ActorError::from).and_then(|r| r),
                    Err(error) => Err(ActorError::from(error)),
                };
                if let Err(error) = result {
                    control.fail(error);
                }
                finished.finish(result);
            })
            .map_err(ActorError::from)?;
            drop(observer);
            Ok::<(), ActorError>(())
        })
        .await
        .map_err(ActorError::from)?;
    admission.wait().await.map_err(ActorError::from)??;
    Ok((owner, address))
}
// Each local handler gets an S02 root, so proxy finalizers and !Send
// registered resources are confirmed per request, not only at actor shutdown.
struct LocalState<A: Actor>(std::rc::Rc<std::cell::RefCell<Option<A>>>);
impl<A: Actor> Actor for LocalState<A> {}
impl<A: Actor> Envelope<LocalState<A>> for LocalEnvelope<A> {
    type Future<'a> = LocalBoxFuture<'a, Result<(), ActorError>>;
    fn handle<'a>(self, actor: &'a mut LocalState<A>, context: ActorContext) -> Self::Future<'a> {
        let state = actor.0.clone();
        Box::pin(async move {
            let task = super::super::spawn_local(async move {
                // The serial dispatcher transfers exclusive ownership for
                // this request; no RefCell borrow survives an await. A panic
                // drops the invalid body in this S02 root and stops the actor.
                let mut actor = state.borrow_mut().take().expect("serial actor state");
                let result = (self.0)(&mut actor, context).await;
                state.borrow_mut().replace(actor);
                result
            })
            .map_err(ActorError::from)?;
            task.wait().await.map_err(ActorError::from)?
        })
    }
}
struct Driver<E> {
    receiver: mpsc::Receiver<Packet<E>>,
    queue: VecDeque<Packet<E>>,
    cleanup: FuturesUnordered<BoxFuture<'static, Result<(), ActorError>>>,
    control: Arc<Control>,
}
impl<E: Payload> Driver<E> {
    fn maintain(&mut self) {
        let state = self.control.state.lock().unwrap();
        // A task's completion is published after its body and cleanup release.
        let closing = state.mode == Mode::Closing;
        let error = state.failure.unwrap_or(ActorError::Cancelled);
        drop(state);
        let mut index = 0;
        while index < self.queue.len() {
            if closing || self.queue[index].request.token.is_cancelled() {
                self.cleanup.push(Box::pin(discard(
                    self.queue.remove(index).unwrap(),
                    error,
                    true,
                )));
            } else {
                index += 1;
            }
        }
    }
    fn poll_aux(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Poll::Ready(Some(result)) = Pin::new(&mut self.cleanup).poll_next(cx) {
            if let Err(error) = result {
                self.control.fail(error);
            }
            return Poll::Ready(());
        }
        // Poll completion signals: executor loss must be observable as failure.
        let mut failure = None;
        let mut progress = false;
        {
            let mut state = self.control.state.lock().unwrap();
            state
                .managed
                .retain_mut(|task| match Pin::new(&mut task.signal).poll(cx) {
                    Poll::Ready(result) => {
                        if let Err(error) = result.unwrap_or(Err(ActorError::ExecutorStopped)) {
                            failure = Some(error);
                        }
                        progress = true;
                        false
                    }
                    Poll::Pending => true,
                });
        }
        if let Some(error) = failure {
            self.control.fail(error);
        }
        if progress {
            return Poll::Ready(());
        }
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(packet)) => {
                self.queue.push_back(packet);
                Poll::Ready(())
            }
            _ => Poll::Pending,
        }
    }
    async fn drive<A: Actor>(&mut self, actor: &mut A) -> Result<(), ActorError>
    where
        E: Envelope<A>,
    {
        loop {
            self.maintain();
            if let Some(packet) = self.queue.pop_front() {
                let request = packet.request.clone();
                let context = ActorContext {
                    token: request.token.clone(),
                    control: self.control.clone(),
                    request: request.clone(),
                };
                let mut handler = Box::pin(execute(actor, packet, context));
                loop {
                    self.maintain();
                    if self.control.cancelling.is_cancelled() {
                        request.token.cancel();
                    }
                    let control = self.control.clone();
                    let mut changed = Box::pin(control.changed.notified());
                    let event = poll_fn(|cx| {
                        if changed.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(None);
                        }
                        // Never cancel/drop this future because a request token
                        // fired: only the handler itself decides how to recover.
                        if let Poll::Ready(result) = handler.as_mut().poll(cx) {
                            return Poll::Ready(Some(result));
                        }
                        self.poll_aux(cx).map(|()| None)
                    })
                    .await;
                    if let Some(result) = event {
                        if let Err(error) = result {
                            self.control.fail(error);
                        }
                        break;
                    }
                }
                continue;
            }
            let control = self.control.clone();
            let mut changed = Box::pin(control.changed.notified());
            // Poll receiver before testing for drain completion: a sender may
            // have committed a packet whose owner deque has not seen it yet.
            let end = poll_fn(|cx| {
                if changed.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(false);
                }
                if self.poll_aux(cx).is_ready() {
                    return Poll::Ready(false);
                }
                let mut state = control.state.lock().unwrap();
                if state.mode != Mode::Running
                    && self.queue.is_empty()
                    && self.cleanup.is_empty()
                    && state.managed.is_empty()
                {
                    // Admission shares this lock; no tracked continuation can
                    // appear after this final drain decision.
                    state.mode = Mode::Closing;
                    self.receiver.close();
                    control.slots.close();
                    return Poll::Ready(true);
                }
                Poll::Pending
            })
            .await;
            if end {
                return self
                    .control
                    .state
                    .lock()
                    .unwrap()
                    .failure
                    .map_or(Ok(()), Err);
            }
        }
    }
}
async fn execute<A: Actor, E: Envelope<A>>(
    actor: &mut A,
    packet: Packet<E>,
    context: ActorContext,
) -> Result<(), ActorError> {
    let Packet {
        envelope,
        request,
        finished,
        permit,
    } = packet;
    drop(permit); // handler has started; only pending work consumes mailbox space
    let scope = CallContext::new(CancellationToken::new());
    let call = scope.run(async move { envelope.handle(actor, context).await });
    let result = AssertUnwindSafe(call).catch_unwind().await;
    let cleanup = scope.wait_closed().await;
    let result = match (result, cleanup) {
        (Ok(Ok(result)), Ok(())) => result,
        _ => Err(ActorError::Panicked),
    };
    let outcome = result.and_then(|()| {
        // Share close's admission lock. A close accepted during one synchronous
        // handler poll must win too; propagation before the next poll is not
        // sufficient. Later cancellation cannot replace this committed outcome.
        let state = request.control.state.lock().unwrap();
        if let Some(error) = state.failure {
            Err(error)
        } else if state.mode == Mode::Closing || request.token.is_cancelled() {
            Err(ActorError::Cancelled)
        } else {
            Ok(())
        }
    });
    let reply = request.reply.lock().unwrap().take();
    let result = if let Some(reply) = reply {
        let delivery = CallContext::new(CancellationToken::new());
        let delivered = AssertUnwindSafe(delivery.run(async move { reply(outcome.is_ok()).await }))
            .catch_unwind()
            .await;
        let cleanup = delivery.wait_closed().await;
        match (delivered, cleanup) {
            (Ok(Ok(delivered)), Ok(())) => delivered.and(result),
            _ => Err(ActorError::Panicked),
        }
    } else {
        result
    };
    finished.finish(result.and(outcome));
    result
}
async fn run<A: Actor, E: Envelope<A>>(
    mut actor: A,
    mut driver: Driver<E>,
) -> Result<(), ActorError> {
    let control = driver.control.clone();
    let scope = CallContext::new(CancellationToken::new());
    let result = AssertUnwindSafe(scope.run(async move {
        let result = driver.drive(&mut actor).await;
        drop(actor);
        result
    }))
    .catch_unwind()
    .await;
    let cleanup = scope.wait_closed().await;
    let result = match (result, cleanup) {
        (Ok(Ok(result)), Ok(())) => result,
        _ => Err(ActorError::Panicked),
    };
    if let Err(error) = result {
        control.fail(error);
    }
    result
}
