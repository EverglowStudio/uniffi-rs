#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]
use std::{cell::Cell, rc::Rc};
use uniffi_core::async_runtime::{actor::*, BoxFuture, LocalBoxFuture, LocalExecutor};
struct Read;
impl Message for Read {
    type Reply = usize;
}
struct SendState(Cell<usize>);
impl Actor for SendState {}
impl Handler<Read> for SendState {
    type Future<'a> = BoxFuture<'a, usize>;
    fn handle<'a>(&'a mut self, _: Read, _: ActorContext) -> Self::Future<'a> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.0.get()
        })
    }
}
struct LocalState(Rc<Cell<usize>>);
impl Actor for LocalState {}
impl Handler<Read> for LocalState {
    type Future<'a> = LocalBoxFuture<'a, usize>;
    fn handle<'a>(&'a mut self, _: Read, _: ActorContext) -> Self::Future<'a> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.0.get()
        })
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
#[test]
fn send_not_sync_and_owner_local_share_handler_trait() {
    runtime().block_on(async {
        let (owner, address) = start(SendState(Cell::new(12)), 2).unwrap();
        assert_eq!(address.ask(Read).await, Ok(12));
        owner.close().await.unwrap();
        let executor = LocalExecutor::new(4).unwrap();
        let (owner, address) = start_local(&executor, || LocalState(Rc::new(Cell::new(23))), 2)
            .await
            .unwrap();
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        assert_send_sync(&address);
        assert_eq!(address.ask(Read).await, Ok(23));
        owner.close().await.unwrap();
        executor.close().await.unwrap();
    });
}

use futures::{channel::oneshot, poll};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use uniffi_core::async_runtime::{current_context, CallContext, CancellationToken};
struct TestActor {
    value: Cell<usize>,
    drops: Arc<AtomicUsize>,
}
impl Actor for TestActor {}
impl Drop for TestActor {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
type Reply = Result<usize, &'static str>;
type CommandHandler =
    Box<dyn for<'a> FnOnce(&'a mut TestActor, ActorContext) -> BoxFuture<'a, Reply> + Send>;
struct Command(CommandHandler);
impl Message for Command {
    type Reply = Reply;
}
impl Handler<Command> for TestActor {
    type Future<'a> = BoxFuture<'a, Reply>;
    fn handle<'a>(&'a mut self, message: Command, context: ActorContext) -> Self::Future<'a> {
        (message.0)(self, context)
    }
}
fn command(
    f: impl for<'a> FnOnce(&'a mut TestActor, ActorContext) -> BoxFuture<'a, Reply> + Send + 'static,
) -> Command {
    Command(Box::new(f))
}
fn add(n: usize) -> Command {
    command(move |actor, _| {
        Box::pin(async move {
            actor.value.set(actor.value.get() + n);
            Ok(actor.value.get())
        })
    })
}
fn test_actor(capacity: usize) -> (ActorOwner, ActorRef<TestActor>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let (owner, address) = start(
        TestActor {
            value: Cell::new(0),
            drops: drops.clone(),
        },
        capacity,
    )
    .unwrap();
    (owner, address, drops)
}
struct Probe {
    dropped: Option<oneshot::Sender<std::thread::ThreadId>>,
    count: Arc<AtomicUsize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        if let Some(sender) = self.dropped.take() {
            let _ = sender.send(std::thread::current().id());
        }
    }
}
#[test]
fn fifo_bound_business_error_and_drain() {
    runtime().block_on(async {
        let (owner, address, drops) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut first = address.ask(command(move |actor, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
                actor.value.set(1);
                Ok(1)
            })
        }));
        assert!(poll!(&mut first).is_pending());
        ready.await.unwrap();
        let mut second = address.ask(add(2));
        assert!(poll!(&mut second).is_pending());
        assert!(matches!(
            address.try_send(add(100)),
            Err(TrySendError::Full(_))
        ));
        let mut third = Box::pin(address.tell(add(4)));
        assert!(poll!(&mut third).is_pending());
        let mut drain = Box::pin(owner.drain_and_close());
        assert!(poll!(&mut drain).is_pending());
        assert_eq!(third.await, Err(ActorError::Closed));
        release.send(()).unwrap();
        assert_eq!(first.await, Ok(Ok(1)));
        assert_eq!(second.await, Ok(Ok(3)));
        drain.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(matches!(
            address.try_send(add(1)),
            Err(TrySendError::Closed(_))
        ));
        assert_eq!(address.ask(add(1)).await, Err(ActorError::Closed));
        let (owner, address, _) = test_actor(1);
        assert_eq!(
            address
                .ask(command(|_, _| Box::pin(async { Err("business") })))
                .await,
            Ok(Err("business"))
        );
        assert_eq!(address.ask(add(9)).await, Ok(Ok(9)));
        owner.close().await.unwrap();
    });
}
#[test]
fn waiting_cancel_never_admitted_and_queued_cancel_releases_while_handler_blocked() {
    runtime().block_on(async {
        let (owner, address, _) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut first = address.ask(command(move |_, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
                Ok(0)
            })
        }));
        assert!(poll!(&mut first).is_pending());
        ready.await.unwrap();
        let (dropped, released) = oneshot::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let probe = Probe {
            dropped: Some(dropped),
            count: count.clone(),
        };
        let mut queued = address.ask(command(move |_, _| {
            let _probe = probe;
            panic!("cancelled queued handler executed")
        }));
        assert!(poll!(&mut queued).is_pending());
        let mut waiting = address.ask(add(100));
        assert!(poll!(&mut waiting).is_pending());
        waiting.cancel();
        assert_eq!(waiting.await, Err(ActorError::Cancelled));
        queued.cancel();
        released.await.unwrap(); // must happen while first handler is still gated
        assert_eq!(queued.await, Err(ActorError::Cancelled));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        assert_eq!(first.await, Ok(Ok(0)));
        assert_eq!(address.ask(add(1)).await, Ok(Ok(1)));
        owner.close().await.unwrap();
    });
}
#[test]
fn started_handler_cooperates_and_ffi_drop_waits_for_recovery_and_cleanup() {
    runtime().block_on(async {
        let (owner, address, _) = test_actor(2);
        let (started, ready) = oneshot::channel();
        let (noticed, cancellation_seen) = oneshot::channel();
        let (release, recovery) = oneshot::channel();
        let (clean, cleanup_gate) = oneshot::channel();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(context.run(address.ask(command(move |actor, ctx| {
            Box::pin(async move {
                actor.value.set(10);
                current_context()
                    .unwrap()
                    .defer_cleanup(async {
                        cleanup_gate.await.unwrap();
                    })
                    .unwrap();
                started.send(()).unwrap();
                ctx.cancelled().await;
                noticed.send(()).unwrap();
                recovery.await.unwrap();
                actor.value.set(20);
                Ok(20)
            })
        }))));
        assert!(poll!(&mut outer).is_pending());
        ready.await.unwrap();
        drop(outer);
        cancellation_seen.await.unwrap();
        let mut closed = Box::pin(context.wait_closed());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(poll!(&mut closed).is_pending());
        clean.send(()).unwrap();
        closed.await.unwrap();
        assert_eq!(address.ask(add(1)).await, Ok(Ok(21)));
        owner.close().await.unwrap();
    });
}
#[test]
fn shared_close_waiter_drop_and_drain_upgrade_preserve_confirmation() {
    runtime().block_on(async {
        let (owner, address, drops) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let (noticed, cancelled) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut request = address.ask(command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                noticed.send(()).unwrap();
                gate.await.unwrap();
                Ok(0)
            })
        }));
        assert!(poll!(&mut request).is_pending());
        ready.await.unwrap();
        let mut queued = address.ask(add(99));
        assert!(poll!(&mut queued).is_pending());
        let mut drain = Box::pin(owner.drain_and_close());
        assert!(poll!(&mut drain).is_pending());
        let context = CallContext::new(CancellationToken::new());
        let mut close = Box::pin(context.run(owner.close()));
        assert!(poll!(&mut close).is_pending());
        drop(close);
        cancelled.await.unwrap();
        assert_eq!(queued.await, Err(ActorError::Cancelled));
        let mut cleanup = Box::pin(context.wait_closed());
        assert!(poll!(&mut cleanup).is_pending());
        assert!(poll!(&mut drain).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert_eq!(request.await, Err(ActorError::Cancelled));
        cleanup.await.unwrap();
        drain.await.unwrap();
        owner.close().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn drain_tracks_necessary_return_and_cancels_timer() {
    runtime().block_on(async {
        let (owner, address, drops) = test_actor(1);
        let (release, gate) = oneshot::channel();
        let (reported, result) = oneshot::channel();
        let return_address = address.clone();
        assert_eq!(
            address
                .ask(command(move |_, ctx| Box::pin(async move {
                    ctx.spawn(move |task| async move {
                        gate.await.unwrap();
                        task.send(
                            &return_address,
                            command(move |actor, _| {
                                Box::pin(async move {
                                    actor.value.set(55);
                                    reported.send(actor.value.get()).unwrap();
                                    Ok(55)
                                })
                            }),
                        )
                        .await
                    })
                    .unwrap();
                    Ok(0)
                })))
                .await,
            Ok(Ok(0))
        );
        address
            .notify_after(std::time::Duration::from_secs(3600), add(999))
            .unwrap();
        assert_eq!(owner.managed_count(), 2);
        let mut drain = Box::pin(owner.drain_and_close());
        assert!(poll!(&mut drain).is_pending());
        tokio::task::yield_now().await;
        assert!(poll!(&mut drain).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            address.notify_after(std::time::Duration::ZERO, add(1)),
            Err(ActorError::Closed)
        );
        release.send(()).unwrap();
        assert_eq!(result.await.unwrap(), 55);
        drain.await.unwrap();
        assert_eq!(owner.managed_count(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn close_unblocks_managed_return_waiting_on_full_mailbox() {
    runtime().block_on(async {
        let (owner, address, _) = test_actor(1);
        let (release_task, task_gate) = oneshot::channel();
        let (blocked, at_send) = oneshot::channel();
        let (task_done, task_result) = oneshot::channel();
        let return_address = address.clone();
        address
            .ask(command(move |_, ctx| {
                Box::pin(async move {
                    ctx.spawn(move |task| async move {
                        task_gate.await.unwrap();
                        let mut send = Box::pin(task.send(&return_address, add(100)));
                        assert!(poll!(&mut send).is_pending());
                        blocked.send(()).unwrap();
                        let result = send.await;
                        task_done.send(result).unwrap();
                        result
                    })
                    .unwrap();
                    Ok(0)
                })
            }))
            .await
            .unwrap()
            .unwrap();
        let (started, ready) = oneshot::channel();
        let mut current = address.ask(command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                Ok(0)
            })
        }));
        assert!(poll!(&mut current).is_pending());
        ready.await.unwrap();
        address.tell(add(1)).await.unwrap();
        release_task.send(()).unwrap();
        at_send.await.unwrap();
        owner.close().await.unwrap();
        assert_eq!(task_result.await.unwrap(), Err(ActorError::Closed));
        assert_eq!(current.await, Err(ActorError::Cancelled));
        assert_eq!(owner.managed_count(), 0);
    });
}
#[test]
fn panic_terminates_owner_and_pending_asks_without_replay() {
    runtime().block_on(async {
        let (owner, address, drops) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut panics = address.ask(command(move |_, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
                panic!("handler failed")
            })
        }));
        assert!(poll!(&mut panics).is_pending());
        ready.await.unwrap();
        let mut queued = address.ask(add(100));
        assert!(poll!(&mut queued).is_pending());
        release.send(()).unwrap();
        assert_eq!(panics.await, Err(ActorError::Panicked));
        assert_eq!(queued.await, Err(ActorError::Panicked));
        assert_eq!(owner.wait().await, Err(ActorError::Panicked));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(address.ask(add(100)).await, Err(ActorError::Closed));
    });
}
#[test]
fn managed_failure_cancels_current_and_reports_runtime_failure() {
    runtime().block_on(async {
        let (owner, address, _) = test_actor(1);
        let (release, gate) = oneshot::channel();
        address
            .ask(command(move |_, ctx| {
                Box::pin(async move {
                    ctx.spawn(move |_| async move {
                        gate.await.unwrap();
                        Err(ActorError::Panicked)
                    })
                    .unwrap();
                    Ok(0)
                })
            }))
            .await
            .unwrap()
            .unwrap();
        let (started, ready) = oneshot::channel();
        let mut current = address.ask(command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                Ok(1)
            })
        }));
        assert!(poll!(&mut current).is_pending());
        ready.await.unwrap();
        release.send(()).unwrap();
        assert_eq!(current.await, Err(ActorError::Panicked));
        assert_eq!(owner.wait().await, Err(ActorError::Panicked));
    });
}
#[test]
fn normal_completion_is_stable_against_late_cancel_and_owner_drop_closes() {
    runtime().block_on(async {
        let (owner, address, drops) = test_actor(1);
        let mut ask = address.ask(add(2));
        assert_eq!((&mut ask).await, Ok(Ok(2)));
        ask.cancel();
        drop(ask);
        let waiter = owner.clone();
        drop(owner);
        assert!(!address.is_closed());
        waiter.close().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let (owner, address, drops) = test_actor(1);
        drop(owner);
        assert!(address.is_closed());
        // Completion of the retained address's rejected ask does not own actor
        // lifetime; wait for the actual destructor signal deterministically.
        assert_eq!(address.ask(add(2)).await, Err(ActorError::Closed));
        while drops.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    });
}
#[test]
fn capacity_validation_and_stopped_executor_are_explicit() {
    assert!(matches!(
        start(SendState(Cell::new(1)), 0),
        Err(ActorError::InvalidCapacity)
    ));
    let rt = runtime();
    let (owner, address) = rt.block_on(async { start(SendState(Cell::new(1)), 1).unwrap() });
    drop(rt);
    futures::executor::block_on(async {
        assert_eq!(owner.wait().await, Err(ActorError::ExecutorStopped));
        assert!(matches!(
            address.try_send(Read),
            Err(TrySendError::Closed(_))
        ));
    });
}

struct LocalTest {
    value: Rc<Cell<usize>>,
    dropped: Option<oneshot::Sender<std::thread::ThreadId>>,
}
impl Actor for LocalTest {}
impl Drop for LocalTest {
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take() {
            let _ = sender.send(std::thread::current().id());
        }
    }
}
type LocalCommandHandler =
    Box<dyn for<'a> FnOnce(&'a mut LocalTest, ActorContext) -> LocalBoxFuture<'a, usize> + Send>;
struct LocalCommand(LocalCommandHandler);
impl Message for LocalCommand {
    type Reply = usize;
}
impl Handler<LocalCommand> for LocalTest {
    type Future<'a> = LocalBoxFuture<'a, usize>;
    fn handle<'a>(&'a mut self, message: LocalCommand, ctx: ActorContext) -> Self::Future<'a> {
        (message.0)(self, ctx)
    }
}
fn local_command(
    f: impl for<'a> FnOnce(&'a mut LocalTest, ActorContext) -> LocalBoxFuture<'a, usize>
        + Send
        + 'static,
) -> LocalCommand {
    LocalCommand(Box::new(f))
}
#[test]
fn local_queue_payload_body_and_async_cleanup_release_on_owner() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (created, owner_thread) = oneshot::channel();
        let (body_drop, body_released) = oneshot::channel();
        let (owner, address) = start_local(
            &executor,
            move || {
                created.send(std::thread::current().id()).unwrap();
                LocalTest {
                    value: Rc::new(Cell::new(0)),
                    dropped: Some(body_drop),
                }
            },
            1,
        )
        .await
        .unwrap();
        let owner_thread = owner_thread.await.unwrap();
        assert_ne!(owner_thread, std::thread::current().id());
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut active = address.ask(local_command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                gate.await.unwrap();
                0
            })
        }));
        assert!(poll!(&mut active).is_pending());
        ready.await.unwrap();
        let (dropped, released) = oneshot::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let probe = Probe {
            dropped: Some(dropped),
            count: count.clone(),
        };
        let mut queued = address.ask(local_command(move |_, _| {
            drop(probe);
            panic!("withdrawn local handler")
        }));
        assert!(poll!(&mut queued).is_pending());
        queued.cancel();
        assert_eq!(released.await.unwrap(), owner_thread);
        assert_eq!(queued.await, Err(ActorError::Cancelled));
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        assert_eq!(active.await, Err(ActorError::Cancelled));
        close.await.unwrap();
        assert_eq!(body_released.await.unwrap(), owner_thread);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        executor.close().await.unwrap();
    });
}
struct LocalResource {
    started: Option<oneshot::Sender<()>>,
    gate: Option<oneshot::Receiver<()>>,
    local: Rc<()>,
}
impl Drop for LocalResource {
    fn drop(&mut self) {
        let started = self.started.take().unwrap();
        let gate = self.gate.take().unwrap();
        let local = self.local.clone();
        uniffi_core::async_runtime::defer_local_cleanup(async move {
            started.send(()).unwrap();
            gate.await.unwrap();
            drop(local);
        })
        .unwrap();
    }
}
#[test]
fn local_request_waits_for_same_owner_proxy_finalizer_root() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (owner, address) = start_local(
            &executor,
            || LocalTest {
                value: Rc::new(Cell::new(0)),
                dropped: None,
            },
            1,
        )
        .await
        .unwrap();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let child_executor = executor.clone();
        let mut request = address.ask(local_command(move |actor, _| {
            Box::pin(async move {
                let object = child_executor
                    .create(move || async move {
                        LocalResource {
                            started: Some(clean_started),
                            gate: Some(gate),
                            local: Rc::new(()),
                        }
                    })
                    .await
                    .unwrap();
                drop(object);
                actor.value.set(7);
                7
            })
        }));
        assert!(poll!(&mut request).is_pending());
        clean_ready.await.unwrap();
        assert!(poll!(&mut request).is_pending());
        release.send(()).unwrap();
        assert_eq!(request.await, Ok(7));
        owner.close().await.unwrap();
        executor.close().await.unwrap();
    });
}
#[test]
fn local_managed_tasks_keep_rc_and_wait_for_cleanup_on_close() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (owner, address) = start_local(
            &executor,
            || LocalTest {
                value: Rc::new(Cell::new(0)),
                dropped: None,
            },
            1,
        )
        .await
        .unwrap();
        let (started, ready) = oneshot::channel();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        address
            .ask(local_command(move |_, ctx| {
                Box::pin(async move {
                    let local = Rc::new(());
                    ctx.spawn_local(move |task| async move {
                        let _resource = LocalResource {
                            started: Some(clean_started),
                            gate: Some(gate),
                            local,
                        };
                        started.send(()).unwrap();
                        task.cancelled().await;
                        Ok(())
                    })
                    .unwrap();
                    0
                })
            }))
            .await
            .unwrap();
        ready.await.unwrap();
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        clean_ready.await.unwrap();
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        executor.close().await.unwrap();
    });
}
#[test]
fn synchronous_transaction_is_not_preempted_and_commits_before_confirmation() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (owner, address) = start_local(
            &executor,
            || LocalTest {
                value: Rc::new(Cell::new(0)),
                dropped: None,
            },
            1,
        )
        .await
        .unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let committed = Arc::new(AtomicUsize::new(0));
        let mark = committed.clone();
        let mut request = address.ask(local_command(move |actor, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                gate.recv().unwrap();
                actor.value.set(42);
                mark.store(42, Ordering::SeqCst);
                42
            })
        }));
        assert!(poll!(&mut request).is_pending());
        ready.await.unwrap();
        request.cancel();
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        assert_eq!(committed.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert_eq!(request.await, Err(ActorError::Cancelled));
        close.await.unwrap();
        assert_eq!(committed.load(Ordering::SeqCst), 42);
        executor.close().await.unwrap();
    });
}

#[test]
fn cancelled_queue_cleanup_retains_capacity_until_release_and_failure_is_reported() {
    struct Cleanup {
        started: Option<oneshot::Sender<()>>,
        gate: Option<oneshot::Receiver<()>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let started = self.started.take().unwrap();
            let gate = self.gate.take().unwrap();
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    started.send(()).unwrap();
                    gate.await.unwrap();
                })
                .unwrap();
        }
    }
    runtime().block_on(async {
        let (owner, address, _) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let mut active = address.ask(command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                Ok(0)
            })
        }));
        assert!(poll!(&mut active).is_pending());
        ready.await.unwrap();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let cleanup = Cleanup {
            started: Some(clean_started),
            gate: Some(gate),
        };
        let mut queued = address.ask(command(move |_, _| {
            drop(cleanup);
            panic!("withdrawn")
        }));
        assert!(poll!(&mut queued).is_pending());
        queued.cancel();
        clean_ready.await.unwrap();
        assert!(poll!(&mut queued).is_pending());
        assert!(matches!(
            address.try_send(add(1)),
            Err(TrySendError::Full(_))
        ));
        release.send(()).unwrap();
        assert_eq!(queued.await, Err(ActorError::Cancelled));
        address.try_send(add(1)).ok().unwrap();
        owner.close().await.unwrap();
        assert_eq!(active.await, Err(ActorError::Cancelled));
        let (owner, address, _) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let mut active = address.ask(command(move |_, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                Ok(0)
            })
        }));
        assert!(poll!(&mut active).is_pending());
        ready.await.unwrap();
        struct BadDrop;
        impl Drop for BadDrop {
            fn drop(&mut self) {
                panic!("payload drop failed");
            }
        }
        let payload = BadDrop;
        let mut queued = address.ask(command(move |_, _| {
            drop(payload);
            panic!("withdrawn")
        }));
        assert!(poll!(&mut queued).is_pending());
        queued.cancel();
        assert_eq!(queued.await, Err(ActorError::Panicked));
        assert_eq!(active.await, Err(ActorError::Panicked));
        assert_eq!(owner.wait().await, Err(ActorError::Panicked));
    });
}
#[test]
fn local_queued_proxy_release_finishes_without_waiting_for_current_handler() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (owner, address) = start_local(
            &executor,
            || LocalTest {
                value: Rc::new(Cell::new(0)),
                dropped: None,
            },
            1,
        )
        .await
        .unwrap();
        let (started, ready) = oneshot::channel();
        let (release_current, current_gate) = oneshot::channel();
        let mut active = address.ask(local_command(move |_, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                current_gate.await.unwrap();
                0
            })
        }));
        assert!(poll!(&mut active).is_pending());
        ready.await.unwrap();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let object = executor
            .create(move || async move {
                LocalResource {
                    started: Some(clean_started),
                    gate: Some(gate),
                    local: Rc::new(()),
                }
            })
            .await
            .unwrap();
        let mut queued = address.ask(local_command(move |_, _| {
            drop(object);
            panic!("withdrawn")
        }));
        assert!(poll!(&mut queued).is_pending());
        queued.cancel();
        clean_ready.await.unwrap();
        assert!(poll!(&mut queued).is_pending());
        release.send(()).unwrap();
        assert_eq!(queued.await, Err(ActorError::Cancelled));
        release_current.send(()).unwrap();
        active.await.unwrap();
        owner.close().await.unwrap();
        executor.close().await.unwrap();
    });
}
#[test]
fn local_body_release_root_is_part_of_close_confirmation() {
    struct Body(uniffi_core::async_runtime::LocalObject<LocalResource>);
    impl Actor for Body {}
    impl Drop for Body {
        fn drop(&mut self) {
            let _ = &self.0;
        }
    }
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let object = executor
            .create(move || async move {
                LocalResource {
                    started: Some(clean_started),
                    gate: Some(gate),
                    local: Rc::new(()),
                }
            })
            .await
            .unwrap();
        let (owner, _address) = start_local(&executor, move || Body(object), 1)
            .await
            .unwrap();
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        clean_ready.await.unwrap();
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn dropping_cancelled_unadmitted_ask_preserves_started_cleanup() {
    struct Cleanup(Option<oneshot::Sender<()>>, Option<oneshot::Receiver<()>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let started = self.0.take().unwrap();
            let gate = self.1.take().unwrap();
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    started.send(()).unwrap();
                    gate.await.unwrap();
                })
                .unwrap();
        }
    }
    runtime().block_on(async {
        let (owner, address, _) = test_actor(1);
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let payload = Cleanup(Some(started), Some(gate));
        let ask = address.ask(command(move |_, _| {
            drop(payload);
            panic!("cancelled before admission")
        }));
        ask.cancel();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(context.run(ask));
        assert!(poll!(&mut outer).is_pending());
        ready.await.unwrap();
        drop(outer);
        let mut closed = Box::pin(context.wait_closed());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        closed.await.unwrap();
        owner.close().await.unwrap();
    });
}

#[test]
fn local_startup_cancel_before_admission_does_not_construct_or_report_false_failure() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let blocked = executor
            .spawn(move || async move {
                started.send(()).unwrap();
                gate.recv().unwrap();
            })
            .await
            .unwrap();
        ready.await.unwrap();
        let made = Arc::new(AtomicUsize::new(0));
        let mark = made.clone();
        let context = CallContext::new(CancellationToken::new());
        let mut startup = Box::pin(context.run(start_local(
            &executor,
            move || {
                mark.fetch_add(1, Ordering::SeqCst);
                LocalTest {
                    value: Rc::new(Cell::new(0)),
                    dropped: None,
                }
            },
            1,
        )));
        assert!(poll!(&mut startup).is_pending());
        drop(startup);
        let mut closed = Box::pin(context.wait_closed());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        closed.await.unwrap();
        blocked.wait().await.unwrap();
        assert_eq!(made.load(Ordering::SeqCst), 0);
        executor.close().await.unwrap();
    });
}
#[test]
fn local_startup_waiter_drop_after_launch_waits_for_body_cleanup() {
    struct Body {
        _resource: LocalResource,
    }
    impl Actor for Body {}
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (created, ready) = oneshot::channel();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let context = CallContext::new(CancellationToken::new());
        let mut startup = Box::pin(context.run(start_local(
            &executor,
            move || {
                created.send(()).unwrap();
                Body {
                    _resource: LocalResource {
                        started: Some(clean_started),
                        gate: Some(gate),
                        local: Rc::new(()),
                    },
                }
            },
            1,
        )));
        assert!(poll!(&mut startup).is_pending());
        ready.await.unwrap();
        drop(startup);
        clean_ready.await.unwrap();
        let mut closed = Box::pin(context.wait_closed());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        closed.await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn last_local_owner_drop_retains_executor_until_cooperative_cleanup() {
    runtime().block_on(async {
        let executor = LocalExecutor::new(8).unwrap();
        let (owner, address) = start_local(
            &executor,
            || LocalTest {
                value: Rc::new(Cell::new(0)),
                dropped: None,
            },
            1,
        )
        .await
        .unwrap();
        let (started, ready) = oneshot::channel();
        let (noticed, cancelled) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut request = address.ask(local_command(move |actor, ctx| {
            Box::pin(async move {
                started.send(()).unwrap();
                ctx.cancelled().await;
                noticed.send(()).unwrap();
                gate.await.unwrap();
                actor.value.set(5);
                5
            })
        }));
        assert!(poll!(&mut request).is_pending());
        ready.await.unwrap();
        drop(executor);
        let context = CallContext::new(CancellationToken::new());
        let mut closing = Box::pin(context.run(async move {
            drop(owner);
        }));
        assert!(poll!(&mut closing).is_pending());
        cancelled.await.unwrap();
        assert!(poll!(&mut closing).is_pending());
        release.send(()).unwrap();
        closing.await.unwrap();
        assert_eq!(request.await, Err(ActorError::Cancelled));
    });
}

// Review R1-01: a typed reply may be the last proxy to an owner-local resource.
struct ReplyResource {
    started: Option<oneshot::Sender<std::thread::ThreadId>>,
    gate: Option<oneshot::Receiver<()>>,
    drops: Arc<AtomicUsize>,
    local: Rc<()>,
}
impl Drop for ReplyResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        let started = self.started.take().unwrap();
        let gate = self.gate.take().unwrap();
        let local = self.local.clone();
        let dropped_on = std::thread::current().id();
        uniffi_core::async_runtime::defer_local_cleanup(async move {
            started.send(dropped_on).unwrap();
            gate.await.unwrap();
            drop(local);
        })
        .unwrap();
    }
}
struct ResourceReply {
    object: uniffi_core::async_runtime::LocalObject<ReplyResource>,
    started: oneshot::Sender<()>,
    wait_cancel: bool,
    cleanup: Option<ReplyCleanup>,
}
struct ReplyCleanup {
    started: oneshot::Sender<()>,
    gate: oneshot::Receiver<()>,
    fail: bool,
}
impl Message for ResourceReply {
    type Reply = uniffi_core::async_runtime::LocalObject<ReplyResource>;
}
impl ResourceReply {
    async fn run(self, context: ActorContext) -> <Self as Message>::Reply {
        self.started.send(()).unwrap();
        if let Some(cleanup) = self.cleanup {
            current_context()
                .unwrap()
                .defer_cleanup(async move {
                    cleanup.started.send(()).unwrap();
                    cleanup.gate.await.unwrap();
                    assert!(
                        !cleanup.fail,
                        "handler cleanup failed after producing reply"
                    );
                })
                .unwrap();
        }
        if self.wait_cancel {
            context.cancelled().await;
        }
        self.object
    }
}
impl Handler<ResourceReply> for TestActor {
    type Future<'a> = BoxFuture<'a, <ResourceReply as Message>::Reply>;
    fn handle<'a>(&'a mut self, message: ResourceReply, context: ActorContext) -> Self::Future<'a> {
        Box::pin(message.run(context))
    }
}
impl Handler<ResourceReply> for LocalTest {
    type Future<'a> = LocalBoxFuture<'a, <ResourceReply as Message>::Reply>;
    fn handle<'a>(&'a mut self, message: ResourceReply, context: ActorContext) -> Self::Future<'a> {
        Box::pin(message.run(context))
    }
}
#[test]
fn review_p1_01_cancelled_reply_waits_for_owner_release_and_success_transfers() {
    runtime().block_on(async {
        let mut early_confirmations = Vec::new();
        for local in [false, true] {
            for cancel in [false, true] {
                let executor = LocalExecutor::new(8).unwrap();
                let (created, owner_thread) = oneshot::channel();
                let (clean_started, clean_ready) = oneshot::channel();
                let (release, gate) = oneshot::channel();
                let drops = Arc::new(AtomicUsize::new(0));
                let mark = drops.clone();
                let object = executor
                    .create(move || async move {
                        created.send(std::thread::current().id()).unwrap();
                        ReplyResource {
                            started: Some(clean_started),
                            gate: Some(gate),
                            drops: mark,
                            local: Rc::new(()),
                        }
                    })
                    .await
                    .unwrap();
                let owner_thread = owner_thread.await.unwrap();
                let (started, ready) = oneshot::channel();
                let message = ResourceReply {
                    object,
                    started,
                    wait_cancel: cancel,
                    cleanup: None,
                };
                let (owner, mut request) = if local {
                    let (owner, address) = start_local(
                        &executor,
                        || LocalTest {
                            value: Rc::new(Cell::new(0)),
                            dropped: None,
                        },
                        1,
                    )
                    .await
                    .unwrap();
                    (owner, address.ask(message))
                } else {
                    let (owner, address, _) = test_actor(1);
                    (owner, address.ask(message))
                };
                assert!(poll!(&mut request).is_pending());
                ready.await.unwrap();
                let pending_before_release;
                if cancel {
                    // No outer CallContext: explicit Rust cancellation must own
                    // the confirmation even while the reply receiver survives.
                    let mut cancelling = Box::pin(request.cancel_and_wait());
                    let (early, dropped_on) =
                        match futures::future::select(clean_ready, &mut cancelling).await {
                            futures::future::Either::Left((thread, _)) => {
                                (poll!(&mut cancelling), thread.unwrap())
                            }
                            futures::future::Either::Right((result, ready)) => {
                                (std::task::Poll::Ready(result), ready.await.unwrap())
                            }
                        };
                    pending_before_release = early.is_pending();
                    assert_eq!(dropped_on, owner_thread);
                    release.send(()).unwrap();
                    let result = match early {
                        std::task::Poll::Ready(result) => result,
                        std::task::Poll::Pending => cancelling.await,
                    };
                    assert!(matches!(result, Err(ActorError::Cancelled)));
                } else {
                    let object = request.await.unwrap();
                    assert_eq!(
                        drops.load(Ordering::SeqCst),
                        0,
                        "successful reply was not transferred"
                    );
                    let mut close = Box::pin(object.close());
                    assert!(poll!(&mut close).is_pending());
                    assert_eq!(clean_ready.await.unwrap(), owner_thread);
                    pending_before_release = poll!(&mut close).is_pending();
                    release.send(()).unwrap();
                    close.await.unwrap();
                }
                owner.close().await.unwrap();
                executor.close().await.unwrap();
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                if !pending_before_release {
                    early_confirmations.push((local, cancel));
                }
            }
        }
        assert!(
            early_confirmations.is_empty(),
            "reply cleanup gate did not delay confirmation: {early_confirmations:?}"
        );
    });
}

#[test]
fn review_p1_02_native_close_accepted_inside_one_poll_cancels_after_sync_cleanup() {
    runtime().block_on(async {
        let (started_actor, actor_ready) = oneshot::channel();
        let worker = std::thread::spawn(move || {
            runtime().block_on(async {
                let (owner, address, _) = test_actor(1);
                started_actor.send((owner.clone(), address)).ok().unwrap();
                owner.wait().await
            })
        });
        let (owner, address) = actor_ready.await.unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (clean_started, clean_ready) = oneshot::channel();
        let (release_cleanup, cleanup_gate) = std::sync::mpsc::channel();
        let committed = Arc::new(AtomicUsize::new(0));
        let mark = committed.clone();
        let mut request = address.ask(command(move |actor, _| {
            Box::pin(async move {
                started.send(()).unwrap();
                gate.recv().unwrap();
                actor.value.set(42);
                mark.store(42, Ordering::SeqCst);
                current_context()
                    .unwrap()
                    .defer_cleanup(async move {
                        clean_started.send(()).unwrap();
                        // Also synchronous: execute must not need another Pending to
                        // observe close. Gates establish the cross-thread order.
                        cleanup_gate.recv().unwrap();
                        mark.store(43, Ordering::SeqCst);
                    })
                    .unwrap();
                Ok(42)
            })
        }));
        assert!(poll!(&mut request).is_pending());
        ready.await.unwrap();
        let mut close = Box::pin(owner.close());
        assert!(poll!(&mut close).is_pending());
        assert!(address.is_closed());
        assert_eq!(committed.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        clean_ready.await.unwrap();
        assert_eq!(committed.load(Ordering::SeqCst), 42);
        assert!(poll!(&mut request).is_pending());
        assert!(poll!(&mut close).is_pending());
        release_cleanup.send(()).unwrap();
        let result = request.await;
        close.await.unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(committed.load(Ordering::SeqCst), 43);
        assert_eq!(result, Err(ActorError::Cancelled));
    });
}

struct RejectedCleanup {
    started: Option<oneshot::Sender<()>>,
    gate: Option<oneshot::Receiver<()>>,
    completed: Arc<AtomicUsize>,
    panic_in_drop: bool,
}
impl Drop for RejectedCleanup {
    fn drop(&mut self) {
        let scope = current_context().unwrap();
        let started = self.started.take().unwrap();
        let gate = self.gate.take().unwrap();
        let completed = self.completed.clone();
        scope
            .defer_cleanup(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
                completed.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        if self.panic_in_drop {
            panic!("rejected payload Drop failure");
        }
        scope
            .defer_cleanup(async { panic!("rejected payload cleanup failure") })
            .unwrap();
    }
}
async fn rejected_cleanup_case(path: u8, panic_in_drop: bool) -> Result<(), ActorError> {
    let (owner, address, _) = test_actor(1);
    let (started, ready) = oneshot::channel();
    let (release_active, active_gate) = oneshot::channel();
    let mut active = address.ask(command(move |_, _| {
        Box::pin(async move {
            started.send(()).unwrap();
            active_gate.await.unwrap();
            Ok(0)
        })
    }));
    assert!(poll!(&mut active).is_pending());
    ready.await.unwrap();
    assert!(address.try_send(add(1)).is_ok());
    let (clean_started, clean_ready) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let completed = Arc::new(AtomicUsize::new(0));
    let payload = RejectedCleanup {
        started: Some(clean_started),
        gate: Some(gate),
        completed: completed.clone(),
        panic_in_drop,
    };
    let message = command(move |_, _| {
        drop(payload);
        panic!("unadmitted handler must not execute")
    });
    let mut rejected: BoxFuture<'_, Result<(), ActorError>> = if path == 2 {
        owner.cancel();
        Box::pin(address.tell(message))
    } else {
        let mut ask = address.ask(message);
        assert!(poll!(&mut ask).is_pending(), "mailbox is full");
        if path == 0 {
            ask.cancel();
        } else {
            owner.cancel();
        }
        Box::pin(async move { ask.await.map(|_| ()) })
    };
    assert!(poll!(&mut rejected).is_pending());
    clean_ready.await.unwrap();
    assert!(poll!(&mut rejected).is_pending());
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    let result = rejected.await;
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    release_active.send(()).unwrap();
    let active_result = active.await;
    if path == 0 {
        assert_eq!(active_result, Ok(Ok(0)));
        assert_eq!(
            address.ask(add(1)).await,
            Ok(Ok(2)),
            "unadmitted cleanup failure must not terminate healthy actor"
        );
    } else {
        assert_eq!(active_result, Err(ActorError::Cancelled));
    }
    owner.close().await.unwrap();
    result
}
#[test]
fn review_p1_03_cancelled_admission_preserves_drop_and_cleanup_failures() {
    runtime().block_on(async {
        let mut results = Vec::new();
        for panic_in_drop in [true, false] {
            results.push(rejected_cleanup_case(0, panic_in_drop).await);
        }
        assert_eq!(results, vec![Err(ActorError::Panicked); 2]);
    });
}
#[test]
fn review_p1_03_closed_admission_preserves_drop_and_cleanup_failures() {
    runtime().block_on(async {
        let mut results = Vec::new();
        for path in [1, 2] {
            for panic_in_drop in [true, false] {
                results.push(rejected_cleanup_case(path, panic_in_drop).await);
            }
        }
        assert_eq!(results, vec![Err(ActorError::Panicked); 4]);
    });
}

#[test]
fn review_p1_01_reply_survives_handler_cleanup_until_failure_or_cancel_is_decided() {
    runtime().block_on(async {
        for local in [false, true] {
            for fail in [false, true] {
                let executor = LocalExecutor::new(8).unwrap();
                let (created, owner_thread) = oneshot::channel();
                let (clean_started, clean_ready) = oneshot::channel();
                let (release, gate) = oneshot::channel();
                let drops = Arc::new(AtomicUsize::new(0));
                let mark = drops.clone();
                let object = executor
                    .create(move || async move {
                        created.send(std::thread::current().id()).unwrap();
                        ReplyResource {
                            started: Some(clean_started),
                            gate: Some(gate),
                            drops: mark,
                            local: Rc::new(()),
                        }
                    })
                    .await
                    .unwrap();
                let (started, ready) = oneshot::channel();
                let (cleanup_started, cleanup_ready) = oneshot::channel();
                let (release_handler, handler_gate) = oneshot::channel();
                let message = ResourceReply {
                    object,
                    started,
                    wait_cancel: false,
                    cleanup: Some(ReplyCleanup {
                        started: cleanup_started,
                        gate: handler_gate,
                        fail,
                    }),
                };
                let (owner, mut request) = if local {
                    let (owner, address) = start_local(
                        &executor,
                        || LocalTest {
                            value: Rc::new(Cell::new(0)),
                            dropped: None,
                        },
                        1,
                    )
                    .await
                    .unwrap();
                    (owner, address.ask(message))
                } else {
                    let (owner, address, _) = test_actor(1);
                    (owner, address.ask(message))
                };
                assert!(poll!(&mut request).is_pending());
                ready.await.unwrap();
                cleanup_ready.await.unwrap();
                // The handler has already returned its value. Cancellation or
                // cleanup failure must still release that value on its owner.
                if !fail {
                    request.cancel();
                }
                assert!(poll!(&mut request).is_pending());
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                release_handler.send(()).unwrap();
                let (early, dropped_on) =
                    match futures::future::select(clean_ready, &mut request).await {
                        futures::future::Either::Left((thread, _)) => {
                            (poll!(&mut request), thread.unwrap())
                        }
                        futures::future::Either::Right((result, ready)) => {
                            (std::task::Poll::Ready(result), ready.await.unwrap())
                        }
                    };
                let pending_before_release = early.is_pending();
                assert_eq!(dropped_on, owner_thread.await.unwrap());
                release.send(()).unwrap();
                let expected = if fail {
                    ActorError::Panicked
                } else {
                    ActorError::Cancelled
                };
                let result = match early {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => request.await,
                };
                assert!(matches!(result, Err(error) if error == expected));
                assert_eq!(
                    owner.close().await,
                    if fail {
                        Err(ActorError::Panicked)
                    } else {
                        Ok(())
                    }
                );
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                executor.close().await.unwrap();
                assert!(
                    pending_before_release,
                    "local={local}, fail={fail}: reply cleanup was not confirmed"
                );
            }
        }
    });
}
