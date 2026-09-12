#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]

use futures_channel::oneshot;
use futures_util::poll;
use std::{cell::Cell, future::pending, rc::Rc, thread, time::Duration};
use uniffi_core::async_runtime::{
    defer_local_cleanup, CallContext, CancellationToken, LocalError, LocalExecutor, TaskError,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Affine {
    value: Rc<Cell<u32>>,
    thread: thread::ThreadId,
    dropped: Option<oneshot::Sender<thread::ThreadId>>,
}
impl Drop for Affine {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.thread);
        self.dropped.take().unwrap().send(self.thread).unwrap();
    }
}

#[test]
fn borrowed_runtime_drives_timers_but_keeps_local_state_and_spawn_on_owner() {
    runtime().block_on(async {
        let caller = thread::current().id();
        let executor = LocalExecutor::with_runtime(4, tokio::runtime::Handle::current()).unwrap();
        let (dropped, destroyed) = oneshot::channel();
        let object = executor
            .create(move || async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Affine {
                    value: Rc::new(Cell::new(7)),
                    thread: thread::current().id(),
                    dropped: Some(dropped),
                }
            })
            .await
            .unwrap();
        let (owner, value) = object
            .call(|state| {
                Box::pin(async move {
                    let value = state.value.clone();
                    let owner = state.thread;
                    let result = tokio::task::spawn_local(async move {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        assert_eq!(thread::current().id(), owner);
                        value.set(value.get() + 1);
                        value.get()
                    })
                    .await
                    .unwrap();
                    assert_eq!(thread::current().id(), owner);
                    (owner, result)
                })
            })
            .await
            .unwrap();
        assert_ne!(owner, caller);
        assert_eq!(value, 8);
        object.close().await.unwrap();
        assert_eq!(destroyed.await.unwrap(), owner);
        executor.close().await.unwrap();
        // Closing an owner releases no part of the borrowed runtime's lifetime.
        assert_eq!(
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                42
            })
            .await
            .unwrap(),
            42
        );
    });
}

#[test]
fn cancelled_borrowed_owner_close_waiter_transfers_timer_and_rc_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::with_runtime(4, tokio::runtime::Handle::current()).unwrap();
        let (started, ready) = oneshot::channel();
        let (cleaning, cleanup_started) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let task = executor
            .spawn(move || async move {
                let owner = thread::current().id();
                let local = Rc::new(Cell::new(1));
                defer_local_cleanup(async move {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    assert_eq!(thread::current().id(), owner);
                    cleaning.send(owner).unwrap();
                    gate.await.unwrap();
                    local.set(local.get() + 1);
                    assert_eq!(thread::current().id(), owner);
                    assert_eq!(local.get(), 2);
                })
                .unwrap();
                started.send(owner).unwrap();
                pending::<()>().await;
            })
            .await
            .unwrap();
        let owner = ready.await.unwrap();
        let context = CallContext::new(CancellationToken::new());
        let closing_executor = executor.clone();
        let mut close = Box::pin(context.run(async move { closing_executor.close().await }));
        assert!(poll!(&mut close).is_pending());
        assert_eq!(cleanup_started.await.unwrap(), owner);
        drop(close);
        let mut confirmed = Box::pin(context.wait_closed());
        assert!(poll!(&mut confirmed).is_pending());
        release.send(()).unwrap();
        confirmed.await.unwrap();
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
        executor.close().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    });
}

#[test]
fn borrowed_runtime_rejects_invalid_capacity_before_starting_owner() {
    let runtime = runtime();
    for capacity in [0, tokio::sync::Semaphore::MAX_PERMITS + 1] {
        assert!(matches!(
            LocalExecutor::with_runtime(capacity, runtime.handle().clone()),
            Err(LocalError::InvalidCapacity)
        ));
    }
}
