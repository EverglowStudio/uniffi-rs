use super::super::local::LocalRootCall;
use super::*;
use futures_util::{future::poll_fn, stream::FuturesUnordered, FutureExt, Stream};
#[cfg(all(test, not(target_arch = "wasm32")))]
use std::sync::Mutex;
use std::{
    collections::VecDeque,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Arc,
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
    executor: Option<LocalExecutor>,
    owns_executor: bool,
}
impl OwnerInner {
    fn closed_successfully(&self) -> bool {
        matches!(self.control.done.peek(), Some(Ok(Ok(()))))
            && (!self.owns_executor || self.executor.as_ref().unwrap().closed_successfully())
    }
}
impl Drop for OwnerInner {
    fn drop(&mut self) {
        if self.closed_successfully() {
            return;
        }
        let launched = self.control.stop(Mode::Closing);
        if self.owns_executor {
            let executor = self.executor.as_ref().unwrap().clone();
            if !launched {
                executor.cancel();
            }
            defer_owned(OwnedCompletion {
                actor: launched.then(|| self.control.done.clone()),
                executor,
            });
        } else if launched {
            defer(self.control.done.clone());
        }
    }
}
// An owned executor has a second release boundary. Keep both acknowledgements
// in one Drop handoff even when the close waiter is cancelled during actor drain.
struct OwnedCompletion {
    actor: Option<Signal>,
    executor: LocalExecutor,
}
impl OwnedCompletion {
    async fn wait(&self) -> Result<(), ActorError> {
        let actor = match &self.actor {
            Some(signal) => wait(signal.clone()).await,
            None => Ok(()),
        };
        let executor = self.executor.close().await.map_err(ActorError::from);
        actor.and(executor)
    }
}
fn defer_owned(completion: OwnedCompletion) {
    if let Some(context) = super::super::current_context() {
        context
            .defer_cleanup(async move {
                match completion.wait().await {
                    Ok(()) | Err(ActorError::Cancelled) => {}
                    Err(error) => panic!("owned actor cleanup failed: {error}"),
                }
            })
            .expect("current context accepts owned actor completion");
    }
}
struct OwnedWaitGuard(Option<OwnedCompletion>);
impl Drop for OwnedWaitGuard {
    fn drop(&mut self) {
        if let Some(completion) = self.0.take() {
            defer_owned(completion);
        }
    }
}
impl ActorOwner {
    pub fn cancel(&self) {
        if self.inner.closed_successfully() {
            return;
        }
        self.inner.control.stop(Mode::Closing);
    }
    pub async fn close(&self) -> Result<(), ActorError> {
        self.cancel();
        self.wait().await
    }
    pub async fn drain_and_close(&self) -> Result<(), ActorError> {
        if self.inner.closed_successfully() {
            return Ok(());
        }
        self.inner.control.stop(Mode::Draining);
        self.wait().await
    }
    pub async fn wait(&self) -> Result<(), ActorError> {
        if self.inner.closed_successfully() {
            return Ok(());
        }
        if self.inner.owns_executor {
            let mut guard = OwnedWaitGuard(Some(OwnedCompletion {
                actor: Some(self.inner.control.done.clone()),
                executor: self.inner.executor.as_ref().unwrap().clone(),
            }));
            let result = guard.0.as_ref().unwrap().wait().await;
            guard.0 = None;
            result
        } else {
            wait_owned(self.inner.control.done.clone()).await
        }
    }
    pub fn managed_count(&self) -> usize {
        self.inner.control.managed_count()
    }
}
type MailboxSetup<A, E> = (ActorOwner, ActorRef<A, E>, Driver<E>, Finished);
fn mailbox<A: Actor, E: Payload>(
    capacity: usize,
    executor: Option<LocalExecutor>,
    owns_executor: bool,
) -> Result<MailboxSetup<A, E>, ActorError> {
    if capacity == 0 || capacity > Semaphore::MAX_PERMITS {
        return Err(ActorError::InvalidCapacity);
    }
    let (finished, done) = completion();
    let control = Arc::new(Control {
        state: parking_lot::Mutex::new(State {
            launched: executor.is_none(),
            managed: Vec::new(),
        }),
        status: std::sync::atomic::AtomicU8::new(0),
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
                executor,
                owns_executor,
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
    let (owner, address, driver, finished) = mailbox(capacity, None, false)?;
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
    start_local_inner(executor.clone(), factory, capacity, false).await
}
/// Transfer shutdown responsibility for a dedicated executor to this actor.
/// After the actor's state, handlers and managed tasks are released, its existing
/// observer stops the executor. Owner close/drain/wait also await all executor
/// cleanup. Executor clones do not prolong this lifetime.
///
/// Do not await closure from a task running on this executor: closure includes
/// that task. Use `cancel` locally and wait from the external lifecycle owner.
pub async fn start_owned_local<A, F>(
    executor: LocalExecutor,
    factory: F,
    capacity: usize,
) -> Result<(ActorOwner, LocalActorRef<A>), ActorError>
where
    A: Actor,
    F: FnOnce() -> A + SendOutsideWasm + 'static,
{
    start_local_inner(executor, factory, capacity, true).await
}
async fn start_local_inner<A, F>(
    executor: LocalExecutor,
    factory: F,
    capacity: usize,
    owns_executor: bool,
) -> Result<(ActorOwner, LocalActorRef<A>), ActorError>
where
    A: Actor,
    F: FnOnce() -> A + SendOutsideWasm + 'static,
{
    let setup = mailbox(capacity, Some(executor.clone()), owns_executor);
    let (owner, address, driver, finished) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            if owns_executor {
                executor.close().await.map_err(ActorError::from)?;
            }
            return Err(error);
        }
    };
    let control = owner.inner.control.clone();
    let keepalive = executor.clone();
    // The admission task starts an independent observer in one synchronous
    // poll. Cancelling startup after admission cannot cancel that observer or
    // discard the actor's release acknowledgement.
    let admission = executor
        .spawn(move || async move {
            {
                let mut state = control.state.lock();
                if control.status().mode == Mode::Closing {
                    return Err(ActorError::Cancelled);
                }
                state.launched = true;
            }
            let observer = super::super::spawn_local(async move {
                // Last ActorOwner Drop initiates cooperative shutdown; retain
                // the S02 executor until the actor task confirms its releases.
                let task = super::super::spawn_local(async move {
                    run(LocalState(Rc::new(RefCell::new(Some(factory())))), driver).await
                });
                let result = match task {
                    Ok(task) => task.wait().await.map_err(ActorError::from).and_then(|r| r),
                    Err(error) => Err(ActorError::from(error)),
                };
                if let Err(error) = result {
                    control.fail(error);
                }
                if owns_executor {
                    keepalive.cancel();
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
struct LocalState<A: Actor>(Rc<RefCell<Option<A>>>);
impl<A: Actor> Actor for LocalState<A> {}
// The root already owns pinned work that can outlive this waiter. Keep only
// its status adapter inline; dropping it still uses LocalRootCall's handoff.
struct LocalHandler(
    Result<LocalRootCall<Result<(), ActorError>>, LocalBoxFuture<'static, Result<(), ActorError>>>,
);
impl Future for LocalHandler {
    type Output = Result<(), ActorError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.get_mut().0 {
            Ok(root) => Pin::new(root)
                .poll(cx)
                .map(|result| result.map_err(ActorError::from).and_then(|status| status)),
            Err(cleanup) => cleanup.as_mut().poll(cx),
        }
    }
}
impl<A: Actor> Envelope<LocalState<A>> for LocalEnvelope<A> {
    type Future<'a> = LocalHandler;
    fn handle<'a>(self, actor: &'a mut LocalState<A>, context: ActorContext) -> Self::Future<'a> {
        let state = actor.0.clone();
        // execute calls handle and polls the returned root in the same poll.
        LocalHandler((self.0)(state, context))
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
        if self.queue.is_empty() {
            return;
        }
        let status = self.control.status();
        // A task's completion is published after its body and cleanup release.
        let closing = status.mode == Mode::Closing;
        let error = status.failure.unwrap_or(ActorError::Cancelled);
        let mut index = 0;
        while index < self.queue.len() {
            if closing || self.queue[index].request.is_cancelled() {
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
            let mut state = self.control.state.lock();
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
        let control = self.control.clone();
        let mut changed = std::pin::pin!(control.changed.notified());
        loop {
            self.maintain();
            if let Some(packet) = self.queue.pop_front() {
                let request = packet.request.clone();
                let context = ActorContext {
                    request: request.clone(),
                };
                let mut handler = std::pin::pin!(execute(actor, packet, context));
                loop {
                    self.maintain();
                    if self.control.cancelling.is_cancelled() {
                        request.token().cancel();
                    }
                    let event = poll_fn(|cx| {
                        if changed.as_mut().poll(cx).is_ready() {
                            changed.set(control.changed.notified());
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
            // Poll receiver before testing for drain completion: a sender may
            // have committed a packet whose owner deque has not seen it yet.
            let end = poll_fn(|cx| {
                if changed.as_mut().poll(cx).is_ready() {
                    changed.set(control.changed.notified());
                    return Poll::Ready(false);
                }
                if self.poll_aux(cx).is_ready() {
                    return Poll::Ready(false);
                }
                let state = control.state.lock();
                let mut status = control.status();
                if status.mode != Mode::Running
                    && self.queue.is_empty()
                    && self.cleanup.is_empty()
                    && state.managed.is_empty()
                {
                    // Admission shares this lock; no tracked continuation can
                    // appear after this final drain decision.
                    status.mode = Mode::Closing;
                    control.set_status(status);
                    self.receiver.close();
                    control.slots.close();
                    return Poll::Ready(true);
                }
                Poll::Pending
            })
            .await;
            if end {
                return self.control.status().failure.map_or(Ok(()), Err);
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
    let call = async move { envelope.handle(actor, context).await };
    let result = AssertUnwindSafe(call).catch_unwind().await;
    // The built-in envelope confirms its execution scope before returning. Keep
    // the separate await statement above so its temporary drops before this.
    let result = match result {
        Ok(result) => result,
        Err(_panic) => Err(ActorError::Panicked),
    };
    let outcome = result.and_then(|()| {
        // Read close and failure together at the commit point. A close published
        // during one synchronous handler poll must win too; propagation before
        // the next poll is not sufficient. Later cancellation cannot replace
        // this committed outcome.
        let status = request.control.status();
        if let Some(error) = status.failure {
            Err(error)
        } else if status.mode == Mode::Closing || request.is_cancelled() {
            Err(ActorError::Cancelled)
        } else {
            Ok(())
        }
    });
    let reply = request.reply.lock().take();
    let result = if let Some(reply) = reply {
        let delivery = CallContext::fresh();
        let delivered = AssertUnwindSafe(delivery.run(async move { reply(outcome.is_ok()).await }))
            .catch_unwind()
            .await;
        match delivered {
            Ok(Ok(delivered)) => delivered.and(result),
            Ok(Err(_)) => Err(ActorError::Panicked),
            Err(_panic) => {
                let _ = delivery.wait_closed().await;
                Err(ActorError::Panicked)
            }
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
    let scope = CallContext::fresh();
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::{cell::Cell, future::Ready, rc::Rc, thread::ThreadId};

    struct ReadOrPanic(bool);
    impl Message for ReadOrPanic {
        type Reply = usize;
    }
    struct LocalReady {
        value: Rc<Cell<usize>>,
        owner: ThreadId,
        cleanup: Option<(oneshot::Sender<ThreadId>, oneshot::Receiver<()>)>,
    }
    impl Actor for LocalReady {}
    impl Handler<ReadOrPanic> for LocalReady {
        type Future<'a> = Ready<usize>;
        fn handle<'a>(&'a mut self, message: ReadOrPanic, _: ActorContext) -> Self::Future<'a> {
            assert_eq!(std::thread::current().id(), self.owner);
            assert!(super::super::super::current_context().is_some());
            assert!(!message.0, "synchronous handler construction failed");
            self.value.set(self.value.get() + 1);
            std::future::ready(self.value.get())
        }
    }
    impl Drop for LocalReady {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.owner);
            let (started, gate) = self.cleanup.take().unwrap();
            let value = self.value.clone();
            let owner = self.owner;
            super::super::super::defer_local_cleanup(async move {
                assert_eq!(std::thread::current().id(), owner);
                started.send(owner).unwrap();
                gate.await.unwrap();
                drop(value);
            })
            .unwrap();
        }
    }

    #[test]
    fn local_ready_handler_construction_panic_waits_for_owner_cleanup() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let executor = LocalExecutor::new(4).unwrap();
            let (started, cleanup) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let (owner, address) = start_local(
                &executor,
                move || LocalReady {
                    value: Rc::new(Cell::new(0)),
                    owner: std::thread::current().id(),
                    cleanup: Some((started, gate)),
                },
                1,
            )
            .await
            .unwrap();
            assert_eq!(address.ask(ReadOrPanic(false)).await, Ok(1));
            let mut request = address.ask(ReadOrPanic(true));
            assert!(futures_util::poll!(&mut request).is_pending());
            assert_ne!(cleanup.await.unwrap(), std::thread::current().id());
            assert!(futures_util::poll!(&mut request).is_pending());
            let mut closed = std::pin::pin!(owner.wait());
            assert!(futures_util::poll!(&mut closed).is_pending());
            release.send(()).unwrap();
            assert_eq!(request.await, Err(ActorError::Panicked));
            assert_eq!(closed.await, Err(ActorError::Panicked));
            executor.close().await.unwrap();
        });
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod message_scope_tests {
    use super::*;
    use crate::async_runtime::{current_context, defer_local_cleanup, LocalError};
    use futures::poll;
    use std::{
        future::{ready, Ready},
        sync::atomic::{AtomicUsize, Ordering},
        thread::{self, ThreadId},
    };

    #[derive(Clone, Copy)]
    enum Failure {
        None,
        Drop,
        Cleanup,
    }
    struct Capture {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        cleaned: Arc<AtomicUsize>,
        scope: Arc<Mutex<Option<CallContext>>>,
        started: Option<oneshot::Sender<ThreadId>>,
        gate: Option<oneshot::Receiver<()>>,
        failure: Failure,
        local: bool,
    }
    struct Observed {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        cleaned: Arc<AtomicUsize>,
        scope: Arc<Mutex<Option<CallContext>>>,
        started: oneshot::Receiver<ThreadId>,
        release: oneshot::Sender<()>,
    }
    fn capture(failure: Failure, local: bool) -> (Capture, Observed) {
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let scope = Arc::new(Mutex::new(None));
        let (started, observed) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        (
            Capture {
                calls: calls.clone(),
                drops: drops.clone(),
                cleaned: cleaned.clone(),
                scope: scope.clone(),
                started: Some(started),
                gate: Some(gate),
                failure,
                local,
            },
            Observed {
                calls,
                drops,
                cleaned,
                scope,
                started: observed,
                release,
            },
        )
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            let scope = current_context().expect("capture Drop requires its execution scope");
            *self.scope.lock().unwrap() = Some(scope.clone());
            self.drops.fetch_add(1, Ordering::SeqCst);
            let started = self.started.take().unwrap();
            let gate = self.gate.take().unwrap();
            let cleaned = self.cleaned.clone();
            let owner = thread::current().id();
            let cleanup = async move {
                started.send(thread::current().id()).unwrap();
                gate.await.unwrap();
                cleaned.fetch_add(1, Ordering::SeqCst);
            };
            if self.local {
                let local = Rc::new(());
                defer_local_cleanup(async move {
                    assert_eq!(thread::current().id(), owner);
                    cleanup.await;
                    assert_eq!(thread::current().id(), owner);
                    drop(local);
                })
                .unwrap();
            } else {
                scope.defer_cleanup(cleanup).unwrap();
            }
            match self.failure {
                Failure::None => {}
                Failure::Drop => panic!("rejected capture Drop failed"),
                Failure::Cleanup => scope
                    .defer_cleanup(async { panic!("rejected capture cleanup failed") })
                    .unwrap(),
            }
        }
    }
    impl Message for Capture {
        type Reply = ();
    }
    struct ProbeActor;
    impl Actor for ProbeActor {}
    impl Handler<Capture> for ProbeActor {
        type Future<'a> = Ready<()>;
        fn handle<'a>(&'a mut self, message: Capture, _: ActorContext) -> Self::Future<'a> {
            message.calls.fetch_add(1, Ordering::SeqCst);
            ready(())
        }
    }
    struct Admitted<E> {
        _owner: ActorOwner,
        _driver: Driver<E>,
        _finished: Finished,
        packet: Packet<E>,
        context: ActorContext,
        ask: Ask<()>,
    }
    async fn admitted<E: Wrap<ProbeActor, Capture>>(message: Capture) -> Admitted<E> {
        let (owner, address, mut driver, finished) =
            mailbox::<ProbeActor, E>(1, None, false).unwrap();
        let mut ask = address.ask(message);
        assert!(poll!(&mut ask).is_pending());
        let packet = driver.receiver.try_recv().unwrap();
        let request = packet.request.clone();
        let context = ActorContext { request };
        Admitted {
            _owner: owner,
            _driver: driver,
            _finished: finished,
            packet,
            context,
            ask,
        }
    }
    fn expected(failure: Failure, error: ActorError) -> Result<(), ActorError> {
        Err(match failure {
            Failure::None => error,
            Failure::Drop | Failure::Cleanup => ActorError::Panicked,
        })
    }

    #[test]
    fn root_start_error_preserves_unpolled_captures_without_debug_bound() {
        futures::executor::block_on(async {
            let (capture, observed) = capture(Failure::None, false);
            let calls = observed.calls.clone();
            let local = Rc::new(());
            let future = async move {
                calls.fetch_add(1, Ordering::SeqCst);
                drop((capture, local));
                Ok::<(), ActorError>(())
            };
            let failure = match run_local_root(future) {
                Ok(_) => panic!("root unexpectedly admitted without owner"),
                Err(failure) => failure,
            };
            assert_eq!(failure.error, LocalError::NoOwner);
            assert_eq!(format!("{failure:?}"), "RootStartError(NoOwner)");
            assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
            assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
            let RootStartError { error, future } = failure;
            let cleanup = message_scope(async move {
                drop(future);
                Err(ActorError::from(error))
            });
            let mut cleanup = std::pin::pin!(cleanup);
            assert!(poll!(&mut cleanup).is_pending());
            observed.started.await.unwrap();
            assert_eq!(observed.cleaned.load(Ordering::SeqCst), 0);
            observed.release.send(()).unwrap();
            assert_eq!(cleanup.await, Err(ActorError::StartFailed));
            assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
            assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
            assert_eq!(observed.cleaned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn local_no_owner_rejection_waits_for_cleanup_and_failures() {
        futures::executor::block_on(async {
            for failure in [Failure::None, Failure::Drop, Failure::Cleanup] {
                let (capture, observed) = capture(failure, false);
                let mut request = admitted::<LocalEnvelope<ProbeActor>>(capture).await;
                let mut actor = LocalState(Rc::new(RefCell::new(Some(ProbeActor))));
                let call = execute(&mut actor, request.packet, request.context);
                let mut call = std::pin::pin!(call);
                assert!(poll!(&mut call).is_pending());
                observed.started.await.unwrap();
                assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 0);
                assert!(poll!(&mut request.ask).is_pending());
                let scope = observed.scope.lock().unwrap().clone().unwrap();
                let mut closed = std::pin::pin!(scope.wait_closed());
                assert!(poll!(&mut closed).is_pending());
                observed.release.send(()).unwrap();
                assert_eq!(call.await, expected(failure, ActorError::StartFailed));
                assert_eq!(
                    request.ask.await,
                    expected(failure, ActorError::StartFailed)
                );
                assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 1);
                assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
            }
        });
    }

    #[test]
    fn stopped_owner_rejection_waits_for_rc_cleanup_and_ack() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for failure in [Failure::None, Failure::Drop, Failure::Cleanup] {
                let executor = LocalExecutor::new(4).unwrap();
                let (capture, observed) = capture(failure, true);
                let mut request = admitted::<LocalEnvelope<ProbeActor>>(capture).await;
                let (executed, mut execution) = oneshot::channel();
                let stopping = executor.clone();
                let task = executor
                    .spawn(move || async move {
                        // Real owner cleanup keeps running after stop. It drives
                        // the rejected execution while the caller holds its gate.
                        defer_local_cleanup(async move {
                            let mut actor = LocalState(Rc::new(RefCell::new(Some(ProbeActor))));
                            let result = execute(&mut actor, request.packet, request.context).await;
                            executed.send(result).unwrap();
                        })
                        .unwrap();
                        stopping.cancel();
                    })
                    .await
                    .unwrap();
                assert_ne!(observed.started.await.unwrap(), thread::current().id());
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 0);
                assert_eq!(execution.try_recv().unwrap(), None);
                assert!(poll!(&mut request.ask).is_pending());
                let mut closed = std::pin::pin!(executor.close());
                assert!(poll!(&mut closed).is_pending());
                observed.release.send(()).unwrap();
                assert_eq!(
                    execution.await.unwrap(),
                    expected(failure, ActorError::Closed)
                );
                assert_eq!(request.ask.await, expected(failure, ActorError::Closed));
                task.wait().await.unwrap();
                closed.await.unwrap();
                assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
                assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 1);
            }
        });
    }

    #[test]
    fn unpolled_send_and_rejected_local_future_drop_in_scope() {
        futures::executor::block_on(async {
            for local in [false, true] {
                let (capture, observed) = capture(Failure::None, false);
                if local {
                    let request = admitted::<LocalEnvelope<ProbeActor>>(capture).await;
                    let mut actor = LocalState(Rc::new(RefCell::new(Some(ProbeActor))));
                    let future = request.packet.envelope.handle(&mut actor, request.context);
                    assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
                    drop(future);
                } else {
                    let request = admitted::<SendEnvelope<ProbeActor>>(capture).await;
                    let mut actor = ProbeActor;
                    let future = request.packet.envelope.handle(&mut actor, request.context);
                    assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
                    drop(future);
                }
                assert_eq!(observed.calls.load(Ordering::SeqCst), 0);
                assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
                assert!(current_context().is_none());
                let scope = observed.scope.lock().unwrap().clone().unwrap();
                let mut closed = std::pin::pin!(scope.wait_closed());
                assert!(poll!(&mut closed).is_pending());
                observed.started.await.unwrap();
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 0);
                observed.release.send(()).unwrap();
                assert_eq!(closed.await, Ok(()));
                assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
                assert_eq!(observed.cleaned.load(Ordering::SeqCst), 1);
            }
        });
    }
}
