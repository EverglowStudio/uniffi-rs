use super::*;
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

/// Per-handler cooperative cancellation and explicitly owned background work.
/// `current_context()` is a *cleanup* scope whose token is intentionally neutral;
/// use this context's token to cancel I/O without dropping the whole handler.
#[derive(Clone)]
pub struct ActorContext {
    pub(super) token: CancellationToken,
    pub(super) control: Arc<Control>,
    pub(super) request: Arc<mailbox::Request>,
}
impl ActorContext {
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
    /// Spawn necessary work for this accepted handler. Work must own its inputs,
    /// cooperate with its token, and send results via ManagedContext::send.
    pub fn spawn<F, Fut>(&self, factory: F) -> Result<(), ActorError>
    where
        F: FnOnce(ManagedContext) -> Fut + SendOutsideWasm + 'static,
        Fut: Future<Output = Result<(), ActorError>> + SendOutsideWasm + 'static,
    {
        let (context, finished) = register(self.control.clone(), false)?;
        drop(super::super::spawn(async move {
            finished.finish(managed_run(context, factory).await);
        }));
        Ok(())
    }
    /// Owner-local variant; call inside a S02 local actor handler.
    pub fn spawn_local<F, Fut>(&self, factory: F) -> Result<(), ActorError>
    where
        F: FnOnce(ManagedContext) -> Fut + 'static,
        Fut: Future<Output = Result<(), ActorError>> + 'static,
    {
        let (context, finished) = register(self.control.clone(), false)?;
        let task =
            super::super::spawn_local(managed_run(context, factory)).map_err(ActorError::from)?;
        drop(
            super::super::spawn_local(async move {
                finished.finish(task.wait().await.map_err(ActorError::from).and_then(|r| r));
            })
            .map_err(ActorError::from)?,
        );
        Ok(())
    }
    /// Explicit run-level failure. Business Result replies should use Handler.
    pub fn fail(&self, error: ActorError) {
        self.control.fail(error);
    }
}
/// Owned task context. Necessary result messages may finish during drain, and
/// still use the same bounded queue and ordering as ordinary messages.
#[derive(Clone)]
pub struct ManagedContext {
    token: CancellationToken,
    control: Arc<Control>,
    timer: bool,
}
impl ManagedContext {
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
    pub async fn send<A: Actor, E: Envelope<A> + Wrap<A, M>, M: Message>(
        &self,
        address: &ActorRef<A, E>,
        message: M,
    ) -> Result<(), ActorError> {
        // Privileged drain admission is scoped to this task's own actor only.
        let necessary = !self.timer && Arc::ptr_eq(&self.control, &address.control);
        address
            .deliver(message, necessary, Some(self.token.clone()))
            .await
    }
}
fn register(control: Arc<Control>, timer: bool) -> Result<(ManagedContext, Finished), ActorError> {
    let token = CancellationToken::new();
    let (finished, signal) = completion();
    {
        let mut state = control.state.lock().unwrap();
        if state.mode == Mode::Closing || (timer && state.mode != Mode::Running) {
            return Err(ActorError::Closed);
        }
        state.managed.push(Managed {
            token: token.clone(),
            signal,
            timer,
        });
    }
    control.changed.notify_one();
    Ok((
        ManagedContext {
            token,
            control,
            timer,
        },
        finished,
    ))
}
async fn managed_run<F, Fut>(context: ManagedContext, factory: F) -> Result<(), ActorError>
where
    F: FnOnce(ManagedContext) -> Fut,
    Fut: Future<Output = Result<(), ActorError>>,
{
    let control = context.control.clone();
    let token = context.token.clone();
    let scope = CallContext::new(CancellationToken::new());
    let result = AssertUnwindSafe(scope.run(async move { factory(context).await }))
        .catch_unwind()
        .await;
    let cleanup = scope.wait_closed().await;
    let result = match (result, cleanup) {
        (Ok(Ok(Ok(()))), Ok(())) => Ok(()),
        (Ok(Ok(Err(ActorError::Cancelled | ActorError::Closed))), Ok(()))
            if token.is_cancelled() =>
        {
            Ok(())
        }
        (Ok(Ok(Err(error))), Ok(())) => Err(error),
        _ => Err(ActorError::Panicked),
    };
    if let Err(error) = result {
        control.fail(error);
    }
    control.changed.notify_one();
    result
}
impl<A: Actor, E: Envelope<A>> ActorRef<A, E> {
    /// One-shot timer. Drain and close cancel it, including capacity waiting.
    pub fn notify_after<M: Message>(&self, delay: Duration, message: M) -> Result<(), ActorError>
    where
        E: Wrap<A, M>,
    {
        let (context, finished) = register(self.control.clone(), true)?;
        let address = self.clone();
        drop(super::super::spawn(async move {
            finished.finish(
                managed_run(context, move |context| async move {
                    let token = context.cancellation_token();
                    let cancelled = token.cancelled();
                    let sleep = super::super::sleep(delay);
                    futures_util::pin_mut!(cancelled, sleep);
                    match futures_util::future::select(cancelled, sleep).await {
                        futures_util::future::Either::Left(_) => Ok(()),
                        futures_util::future::Either::Right(_) => {
                            context.send(&address, message).await
                        }
                    }
                })
                .await,
            );
        }));
        Ok(())
    }
}
