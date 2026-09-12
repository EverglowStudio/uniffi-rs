#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]

use futures_channel::oneshot;
use futures_util::poll;
use std::{cell::Cell, future::pending, rc::Rc, sync::mpsc, thread};
use uniffi_core::async_runtime::{
    actor::{self, Actor, ActorError},
    defer_local_cleanup, CallContext, CancellationToken, LocalExecutor, TaskError,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Empty;
impl Actor for Empty {}

struct Affine {
    state: Rc<Cell<u32>>,
    cleanup_started: Option<oneshot::Sender<()>>,
    cleanup_gate: Option<oneshot::Receiver<()>>,
}
impl Actor for Affine {}
impl Drop for Affine {
    fn drop(&mut self) {
        let state = self.state.clone();
        let started = self.cleanup_started.take().unwrap();
        let gate = self.cleanup_gate.take().unwrap();
        let owner = thread::current().id();
        defer_local_cleanup(async move {
            started.send(()).unwrap();
            gate.await.unwrap();
            assert_eq!(thread::current().id(), owner);
            state.set(state.get() + 1);
            assert_eq!(state.get(), 8);
        })
        .unwrap();
    }
}

async fn gated_task(
    executor: &LocalExecutor,
) -> (
    uniffi_core::async_runtime::TaskHandle<()>,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
) {
    let (started, ready) = oneshot::channel();
    let (cleaning, cleaned) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let task = executor
        .spawn(move || async move {
            let state = Rc::new(Cell::new(10));
            let owner = thread::current().id();
            defer_local_cleanup(async move {
                cleaning.send(()).unwrap();
                gate.await.unwrap();
                assert_eq!(thread::current().id(), owner);
                state.set(11);
                assert_eq!(state.get(), 11);
            })
            .unwrap();
            started.send(()).unwrap();
            pending::<()>().await;
        })
        .await
        .unwrap();
    ready.await.unwrap();
    (task, cleaned, release)
}

#[test]
fn shared_local_actor_does_not_stop_its_executor() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (owner, _) = actor::start_local(&executor, || Empty, 4).await.unwrap();
        owner.drain_and_close().await.unwrap();
        drop(owner);
        assert_eq!(
            executor.spawn(|| async { 42 }).await.unwrap().wait().await,
            Ok(42)
        );
        executor.close().await.unwrap();
    });
}

#[test]
fn owned_drain_waits_actor_cleanup_before_stopping_and_waiting_executor_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release_task) = gated_task(&executor).await;
        let (started, actor_cleaning) = oneshot::channel();
        let (release_actor, gate) = oneshot::channel();
        let (owner, _) = actor::start_owned_local(
            executor.clone(),
            move || Affine {
                state: Rc::new(Cell::new(7)),
                cleanup_started: Some(started),
                cleanup_gate: Some(gate),
            },
            4,
        )
        .await
        .unwrap();
        let mut close = Box::pin(owner.drain_and_close());
        assert!(poll!(&mut close).is_pending());
        actor_cleaning.await.unwrap();
        let mut cleaned = Box::pin(cleaned);
        assert!(poll!(&mut cleaned).is_pending());
        // Actor destruction has begun but its Rc cleanup still owns the gate.
        // Unrelated executor work must not be cancelled yet.
        executor
            .spawn(|| async {})
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        release_actor.send(()).unwrap();
        cleaned.await.unwrap();
        assert!(poll!(&mut close).is_pending());
        assert!(executor.spawn(|| async {}).await.is_err());
        release_task.send(()).unwrap();
        close.await.unwrap();
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}

#[test]
fn cancelled_or_dropped_owned_close_waiter_transfers_both_acknowledgements() {
    runtime().block_on(async {
        for cancel_context in [false, true] {
            let executor = LocalExecutor::new(4).unwrap();
            let (task, cleaned, release) = gated_task(&executor).await;
            let (owner, _) = actor::start_owned_local(executor, || Empty, 4)
                .await
                .unwrap();
            let context = CallContext::new(CancellationToken::new());
            let closing = owner.clone();
            let mut call = Box::pin(context.run(async move { closing.close().await }));
            assert!(poll!(&mut call).is_pending());
            cleaned.await.unwrap();
            if cancel_context {
                context.cancel();
                assert!(poll!(&mut call).is_pending());
            }
            drop(call);
            let mut confirmed = Box::pin(context.wait_closed());
            assert!(poll!(&mut confirmed).is_pending());
            release.send(()).unwrap();
            confirmed.await.unwrap();
            assert_eq!(task.wait().await, Err(TaskError::Cancelled));
            owner.wait().await.unwrap();
        }
    });
}

#[test]
fn last_owned_actor_drop_transfers_executor_cleanup_to_current_call() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release) = gated_task(&executor).await;
        let (owner, _) = actor::start_owned_local(executor, || Empty, 4)
            .await
            .unwrap();
        let context = CallContext::new(CancellationToken::new());
        let mut call = Box::pin(context.run(async move { drop(owner) }));
        assert!(poll!(&mut call).is_pending());
        cleaned.await.unwrap();
        assert!(poll!(&mut call).is_pending());
        release.send(()).unwrap();
        call.await.unwrap();
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}

#[test]
fn invalid_owned_capacity_waits_executor_cleanup_before_returning_error() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release) = gated_task(&executor).await;
        let mut start = Box::pin(actor::start_owned_local(executor, || Empty, 0));
        assert!(poll!(&mut start).is_pending());
        cleaned.await.unwrap();
        assert!(poll!(&mut start).is_pending());
        release.send(()).unwrap();
        assert!(matches!(start.await, Err(ActorError::InvalidCapacity)));
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}

#[test]
fn cancelling_owned_start_before_admission_never_constructs_actor_and_waits_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release_task) = gated_task(&executor).await;
        let (blocking, blocked) = oneshot::channel();
        let (release_owner, owner_gate) = mpsc::channel();
        let blocker = executor
            .spawn(move || async move {
                blocking.send(()).unwrap();
                owner_gate.recv().unwrap();
            })
            .await
            .unwrap();
        blocked.await.unwrap();
        let context = CallContext::new(CancellationToken::new());
        let mut start = Box::pin(context.run(actor::start_owned_local(
            executor.clone(),
            || -> Empty { panic!("cancelled admission must not construct actor") },
            4,
        )));
        assert!(poll!(&mut start).is_pending());
        context.cancel();
        assert!(poll!(&mut start).is_pending());
        release_owner.send(()).unwrap();
        cleaned.await.unwrap();
        assert!(poll!(&mut start).is_pending());
        release_task.send(()).unwrap();
        assert!(start.await.is_err());
        let _ = blocker.wait().await;
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
        assert!(executor.spawn(|| async {}).await.is_err());
    });
}

#[test]
fn owned_factory_panic_still_waits_for_executor_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release) = gated_task(&executor).await;
        let (owner, _) =
            actor::start_owned_local(executor, || -> Empty { panic!("owned factory failure") }, 4)
                .await
                .unwrap();
        cleaned.await.unwrap();
        let mut closed = Box::pin(owner.wait());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        assert_eq!(closed.await, Err(ActorError::Panicked));
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}

#[test]
fn last_owner_drop_after_actor_confirmation_still_waits_pending_executor_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(4).unwrap();
        let (task, cleaned, release) = gated_task(&executor).await;
        let (owner, _) = actor::start_owned_local(executor, || Empty, 4)
            .await
            .unwrap();
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        cleaned.await.unwrap();
        // Poll after actor completion so its shared acknowledgement has been
        // observed, while the executor still owns an unfinished Rc cleanup.
        assert!(poll!(&mut close).is_pending());
        drop(close);
        let context = CallContext::new(CancellationToken::new());
        let mut call = Box::pin(context.run(async move { drop(owner) }));
        assert!(poll!(&mut call).is_pending());
        release.send(()).unwrap();
        call.await.unwrap();
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}
