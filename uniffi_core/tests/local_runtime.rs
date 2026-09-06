#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]
use futures::{channel::oneshot, future::pending, poll};
use std::{
    cell::Cell,
    future::Future,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, ThreadId},
    time::Duration,
};
use uniffi_core::async_runtime::{
    self as rt, CallContext, CancellationToken, LocalError, LocalExecutor, LocalObject, TaskError,
};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), future)
                .await
                .expect("local runtime test timed out")
        })
}
fn assert_send<T: Send>(_: &T) {}
fn assert_send_sync<T: Send + Sync>() {}
struct Counter {
    value: Rc<Cell<u32>>,
    events: Arc<Mutex<Vec<ThreadId>>>,
    cleanup: Option<oneshot::Receiver<()>>,
}
impl Counter {
    fn mark(&self) {
        self.events.lock().unwrap().push(thread::current().id());
    }
    async fn inc(&self) -> u32 {
        self.mark();
        tokio::task::yield_now().await;
        self.mark();
        self.value.set(self.value.get() + 1);
        self.value.get()
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.mark();
        if let Some(gate) = self.cleanup.take() {
            let value = self.value.clone();
            let events = self.events.clone();
            rt::defer_local_cleanup(async move {
                gate.await.unwrap();
                events.lock().unwrap().push(thread::current().id());
                drop(value);
            })
            .unwrap();
        }
    }
}

#[test]
fn owner_construction_poll_access_drop_and_cleanup_stay_on_one_thread() {
    block_on(async {
        assert_send_sync::<LocalObject<Counter>>();
        let executor = LocalExecutor::new(1).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let copied = events.clone();
        let (release, gate) = oneshot::channel();
        let create = executor.create(move || async move {
            copied.lock().unwrap().push(thread::current().id());
            Counter {
                value: Rc::new(Cell::new(0)),
                events: copied,
                cleanup: Some(gate),
            }
        });
        assert_send(&create);
        let object = create.await.unwrap();
        let call = object.call(|v| Box::pin(v.inc()));
        assert_send(&call);
        assert_eq!(call.await, Ok(1));
        let other = object.clone();
        assert_eq!(
            thread::spawn(move || futures::executor::block_on(other.call(|v| Box::pin(v.inc()))))
                .join()
                .unwrap(),
            Ok(2)
        );
        let mut close = Box::pin(object.close());
        assert!(poll!(&mut close).is_pending());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(poll!(&mut close).is_pending());
        assert_eq!(
            object.call(|_| Box::pin(async {})).await,
            Err(LocalError::Closed)
        );
        release.send(()).unwrap();
        close.await.unwrap();
        object.close().await.unwrap();
        executor.close().await.unwrap();
        let events = events.lock().unwrap();
        assert!(events.len() >= 7);
        assert_ne!(events[0], thread::current().id());
        assert!(events.iter().all(|id| *id == events[0]));
    });
}

#[test]
fn generic_run_accepts_local_future_and_send_not_sync_outputs() {
    block_on(async {
        let value = Rc::new(Cell::new(3));
        assert_eq!(
            rt::run(async {
                value.set(4);
                value.get()
            })
            .await,
            Ok(4)
        );
        let executor = LocalExecutor::new(1).unwrap();
        let handle = executor
            .spawn(|| async {
                let local = Rc::new(Cell::new(7));
                tokio::task::yield_now().await;
                Cell::new(local.get())
            })
            .await
            .unwrap();
        assert_send(&handle);
        assert_eq!(handle.wait().await.unwrap().get(), 7);
        executor.close().await.unwrap();
    });
}

#[test]
fn local_spawn_requires_owner_and_detached_tasks_are_owned_at_shutdown() {
    block_on(async {
        assert!(matches!(
            rt::spawn_local(async {}),
            Err(LocalError::NoOwner)
        ));
        let executor = LocalExecutor::new(1).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let copy = dropped.clone();
        struct DropCount(Arc<AtomicUsize>, Rc<()>);
        impl Drop for DropCount {
            fn drop(&mut self) {
                let _ = &self.1;
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        executor
            .spawn(move || async move {
                let count = DropCount(copy, Rc::new(()));
                drop(
                    rt::spawn_local(async move {
                        let _count = count;
                        pending::<()>().await;
                    })
                    .unwrap(),
                );
                let output = rt::spawn_local(async { Rc::new(12) })
                    .unwrap()
                    .wait()
                    .await
                    .unwrap();
                assert_eq!(*output, 12);
            })
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        executor.close().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn outer_ffi_cancel_waits_for_owner_local_destructor_cleanup() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let copy = dropped.clone();
        let object = executor
            .create(|| async { Rc::new(Cell::new(0)) })
            .await
            .unwrap();
        let context = CallContext::new(CancellationToken::new());
        let mut call = Box::pin(context.run(object.call(move |v| {
            Box::pin(async move {
                let local = v.clone();
                rt::defer_local_cleanup(async move {
                    gate.await.unwrap();
                    drop(local);
                    copy.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
                started.send(()).unwrap();
                pending::<()>().await;
            })
        })));
        assert!(poll!(&mut call).is_pending());
        ready.await.unwrap();
        context.cancel();
        assert!(poll!(&mut call).is_pending());
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert_eq!(call.await, Err(rt::Cancelled));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        object.close().await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn dropping_outer_run_transfers_confirmation_and_nested_local_cleanup() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let object = executor.create(|| async { Rc::new(()) }).await.unwrap();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let copied = cleaned.clone();
        let context = CallContext::new(CancellationToken::new());
        let mut call = Box::pin(context.run(object.call(move |v| {
            Box::pin(async move {
                let local = v.clone();
                rt::defer_local_cleanup(async move {
                    rt::defer_local_cleanup(async move {
                        gate.await.unwrap();
                        drop(local);
                        copied.fetch_add(1, Ordering::SeqCst);
                    })
                    .unwrap();
                })
                .unwrap();
                started.send(()).unwrap();
                pending::<()>().await;
            })
        })));
        assert!(poll!(&mut call).is_pending());
        ready.await.unwrap();
        drop(call);
        let mut closed = Box::pin(context.wait_closed());
        assert!(poll!(&mut closed).is_pending());
        release.send(()).unwrap();
        closed.await.unwrap();
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        object.close().await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn methods_interleave_and_reenter_without_a_registry_lock() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let object = executor
            .create(|| async { Rc::new(Cell::new(0)) })
            .await
            .unwrap();
        let (entered, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut first = Box::pin(object.call(move |v| {
            Box::pin(async move {
                v.set(1);
                entered.send(()).unwrap();
                gate.await.unwrap();
                v.get()
            })
        }));
        assert!(poll!(&mut first).is_pending());
        ready.await.unwrap();
        let again = object.clone();
        assert_eq!(
            object
                .call(move |_| Box::pin(async move {
                    again
                        .call(|v| {
                            Box::pin(async move {
                                v.set(9);
                                v.get()
                            })
                        })
                        .await
                        .unwrap()
                }))
                .await,
            Ok(9)
        );
        release.send(()).unwrap();
        assert_eq!(first.await, Ok(9));
        object.close().await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn capacity_one_cancel_queued_and_close_wakes_capacity_waiter_without_preemption() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (entered, ready) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let blocker = executor
            .spawn(move || async move {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            })
            .await
            .unwrap();
        ready.await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let copied = calls.clone();
        let outer = CallContext::new(CancellationToken::new());
        let mut queued = Box::pin(outer.run(executor.create(move || async move {
            copied.fetch_add(1, Ordering::SeqCst);
            Rc::new(())
        })));
        assert!(poll!(&mut queued).is_pending()); // accepted packet occupies the only slot
        let mut capacity = Box::pin(executor.spawn(|| async {}));
        assert!(poll!(&mut capacity).is_pending());
        outer.cancel();
        assert!(poll!(&mut queued).is_pending());
        let mut close = Box::pin(executor.close());
        assert!(poll!(&mut close).is_pending());
        assert!(matches!(capacity.await, Err(LocalError::Closed)));
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        assert!(matches!(
            blocker.wait().await,
            Ok(()) | Err(TaskError::Cancelled)
        ));
        assert!(matches!(queued.await, Err(rt::Cancelled)));
        close.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn unpolled_cancel_does_not_construct_and_last_proxy_drop_requests_release() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let copied = calls.clone();
        let context = CallContext::new(CancellationToken::new());
        context.cancel();
        assert!(matches!(
            context
                .run(executor.create(move || async move {
                    copied.fetch_add(1, Ordering::SeqCst);
                    Rc::new(())
                }))
                .await,
            Err(rt::Cancelled)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        struct NotifyDrop(Option<oneshot::Sender<ThreadId>>, Rc<()>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                let _ = &self.1;
                self.0.take().unwrap().send(thread::current().id()).unwrap();
            }
        }
        let (dropped, confirmation) = oneshot::channel();
        let object = executor
            .create(move || async move { NotifyDrop(Some(dropped), Rc::new(())) })
            .await
            .unwrap();
        drop(object);
        assert_ne!(confirmation.await.unwrap(), thread::current().id());
        executor.close().await.unwrap();
    });
}

#[test]
fn repeated_owner_close_shares_cleanup_completion_and_reports_panics() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        assert_eq!(
            executor
                .spawn(|| async { panic!("factory business panic") })
                .await
                .unwrap()
                .wait()
                .await,
            Err(TaskError::Panicked)
        );
        assert_eq!(
            executor
                .spawn(|| async {
                    rt::defer_local_cleanup(async { panic!("cleanup panic") }).unwrap();
                })
                .await
                .unwrap()
                .wait()
                .await,
            Err(TaskError::Panicked)
        );
        let (release, gate) = oneshot::channel();
        let (started, ready) = oneshot::channel();
        let task = executor
            .spawn(move || async move {
                let local = Rc::new(());
                rt::defer_local_cleanup(async move {
                    gate.await.unwrap();
                    drop(local);
                })
                .unwrap();
                started.send(()).unwrap();
                pending::<()>().await;
            })
            .await
            .unwrap();
        ready.await.unwrap();
        let mut one = Box::pin(executor.close());
        let mut two = Box::pin(executor.close());
        assert!(poll!(&mut one).is_pending());
        assert!(poll!(&mut two).is_pending());
        drop(one);
        release.send(()).unwrap();
        two.await.unwrap();
        executor.close().await.unwrap();
        assert_eq!(task.wait().await, Err(TaskError::Cancelled));
    });
}

#[test]
fn explicit_object_close_cancels_inflight_method_and_waits_before_destruction() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let copied = events.clone();
        let object = executor
            .create(move || async move {
                Counter {
                    value: Rc::new(Cell::new(0)),
                    events: copied,
                    cleanup: Some(gate),
                }
            })
            .await
            .unwrap();
        let mut method = Box::pin(object.call(move |_| {
            Box::pin(async move {
                started.send(()).unwrap();
                pending::<()>().await
            })
        }));
        assert!(poll!(&mut method).is_pending());
        ready.await.unwrap();
        let mut close = Box::pin(object.close());
        assert!(poll!(&mut close).is_pending());
        assert_eq!(method.await, Err(LocalError::Cancelled));
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        executor.close().await.unwrap();
        assert_eq!(events.lock().unwrap().len(), 2);
    });
}

#[test]
fn invalid_capacity_is_explicit() {
    assert!(matches!(
        LocalExecutor::new(0),
        Err(LocalError::InvalidCapacity)
    ));
}

// Preserve native compile-time constraints without requiring Sync on F/R.
static_assertions::assert_impl_all!(LocalObject<Rc<Cell<u32>>>: Send, Sync);
static_assertions::assert_not_impl_any!(rt::Run<std::future::Ready<Rc<()>>>: Send, Sync);

#[test]
fn last_self_proxy_during_cancel_waits_after_method_leaves_active_set() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (release, gate) = oneshot::channel();
        let (entered, ready) = oneshot::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let copied = events.clone();
        let object = executor
            .create(move || async move {
                Counter {
                    value: Rc::new(Cell::new(0)),
                    events: copied,
                    cleanup: Some(gate),
                }
            })
            .await
            .unwrap();
        let self_proxy = object.clone();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(context.run(async move {
            object
                .call(move |_| {
                    Box::pin(async move {
                        let _last_self_proxy = self_proxy;
                        entered.send(()).unwrap();
                        pending::<()>().await;
                    })
                })
                .await
        }));
        assert!(poll!(&mut outer).is_pending());
        ready.await.unwrap();
        context.cancel();
        assert!(poll!(&mut outer).is_pending());
        release.send(()).unwrap();
        assert_eq!(outer.await, Err(rt::Cancelled));
        assert_eq!(events.lock().unwrap().len(), 2);
        executor.close().await.unwrap();
    });
}

#[test]
fn cancelled_returned_objects_are_released_once_before_ffi_completion() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        for _ in 0..32 {
            let events = Arc::new(Mutex::new(Vec::new()));
            let copied = events.clone();
            let (release, gate) = oneshot::channel();
            let (created, ready) = oneshot::channel();
            let token = CancellationToken::new();
            let context = CallContext::new(token.clone());
            let mut outer = Box::pin(context.run(executor.create(move || async move {
                let value = Counter {
                    value: Rc::new(Cell::new(0)),
                    events: copied,
                    cleanup: Some(gate),
                };
                token.cancel();
                created.send(()).unwrap();
                value
            })));
            assert!(poll!(&mut outer).is_pending());
            ready.await.unwrap();
            assert!(poll!(&mut outer).is_pending());
            release.send(()).unwrap();
            assert!(matches!(outer.await, Err(rt::Cancelled)));
            assert_eq!(events.lock().unwrap().len(), 2);
        }
        executor.close().await.unwrap();
    });
}

#[test]
fn queued_cancel_without_shutdown_never_constructs_factory_and_owner_remains_live() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (entered, ready) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let blocker = executor
            .spawn(move || async move {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            })
            .await
            .unwrap();
        ready.await.unwrap();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(
            context.run(executor.create(|| async { panic!("cancelled queued factory ran") })),
        );
        assert!(poll!(&mut outer).is_pending());
        context.cancel();
        assert!(poll!(&mut outer).is_pending());
        release.send(()).unwrap();
        blocker.wait().await.unwrap();
        assert!(matches!(outer.await, Err(rt::Cancelled)));
        assert_eq!(
            executor.spawn(|| async { 42 }).await.unwrap().wait().await,
            Ok(42)
        );
        executor.close().await.unwrap();
    });
}

#[test]
fn local_task_cancel_before_first_poll_and_last_executor_drop_are_confirmed() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        executor
            .spawn(|| async {
                let child =
                    rt::spawn_local(async { panic!("cancelled unpolled task ran") }).unwrap();
                child.cancel();
                child.cancel();
                assert_eq!(child.wait().await, Err(TaskError::Cancelled));
            })
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let (entered, ready) = oneshot::channel();
        let pending = executor
            .spawn(move || async {
                entered.send(()).unwrap();
                pending::<()>().await
            })
            .await
            .unwrap();
        ready.await.unwrap();
        drop(executor);
        assert_eq!(pending.wait().await, Err(TaskError::Cancelled));
    });
}

struct CloseProbe {
    cleaning: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    cleaned: Arc<AtomicUsize>,
    local: Rc<()>,
}
impl Drop for CloseProbe {
    fn drop(&mut self) {
        let cleaning = self.cleaning.take().unwrap();
        let release = self.release.take().unwrap();
        let cleaned = self.cleaned.clone();
        let local = self.local.clone();
        rt::defer_local_cleanup(async move {
            cleaning.send(()).unwrap();
            release.await.unwrap();
            drop(local);
            cleaned.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    }
}

async fn close_waiter_retains_confirmation(executor_close: bool, drop_waiter: bool) {
    let executor = LocalExecutor::new(1).unwrap();
    let (cleaning, started) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let copied = cleaned.clone();
    let object = executor
        .create(move || async move {
            CloseProbe {
                cleaning: Some(cleaning),
                release: Some(gate),
                cleaned: copied,
                local: Rc::new(()),
            }
        })
        .await
        .unwrap();
    let context = CallContext::new(CancellationToken::new());
    // Keep both public handles alive. Last-proxy Drop must not hide missing
    // confirmation transfer from either close waiter.
    let mut outer = Box::pin(context.run(async {
        if executor_close {
            executor.close().await
        } else {
            object.close().await
        }
    }));
    assert!(poll!(&mut outer).is_pending());
    started.await.unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
    let early = if drop_waiter {
        drop(outer);
        let mut confirmed = Box::pin(context.wait_closed());
        poll!(&mut confirmed).is_ready()
    } else {
        context.cancel();
        context.cancel();
        let early = poll!(&mut outer).is_ready();
        drop(outer);
        early
    };
    assert_eq!(cleaned.load(Ordering::SeqCst), 0);
    // Always release the owner before reporting a red assertion.
    release.send(()).unwrap();
    context.wait_closed().await.unwrap();
    object.close().await.unwrap();
    executor.close().await.unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert!(!early, "close confirmation escaped S01 cleanup: executor_close={executor_close}, drop_waiter={drop_waiter}");
}

#[test]
fn review_p1_object_close_cancellation_waits_for_started_cleanup() {
    block_on(close_waiter_retains_confirmation(false, false));
}
#[test]
fn review_p1_executor_close_cancellation_waits_for_started_cleanup() {
    block_on(close_waiter_retains_confirmation(true, false));
}
#[test]
fn review_p1_object_close_drop_transfers_started_cleanup() {
    block_on(close_waiter_retains_confirmation(false, true));
}
#[test]
fn review_p1_executor_close_drop_transfers_started_cleanup() {
    block_on(close_waiter_retains_confirmation(true, true));
}

#[test]
fn review_p1_nested_last_self_proxy_cancel_does_not_cycle() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (cleaning, final_started) = oneshot::channel();
        let (release_final, final_gate) = oneshot::channel();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let copied = cleaned.clone();
        let object = executor
            .create(move || async move {
                Rc::new(CloseProbe {
                    cleaning: Some(cleaning),
                    release: Some(final_gate),
                    cleaned: copied,
                    local: Rc::new(()),
                })
            })
            .await
            .unwrap();
        let reenter = object.clone();
        let last = object.clone();
        let (entered, ready) = oneshot::channel();
        let (resume_owner, owner_gate) = std::sync::mpsc::channel();
        let (a_cleaning, a_started) = oneshot::channel();
        let (release_a, gate_a) = oneshot::channel();
        let (b_cleaning, b_started) = oneshot::channel();
        let (release_b, gate_b) = oneshot::channel();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(context.run(async move {
            object
                .call(move |a| {
                    Box::pin(async move {
                        let held_by_a_cleanup = a.clone();
                        rt::defer_local_cleanup(async move {
                            a_cleaning.send(()).unwrap();
                            gate_a.await.unwrap();
                            drop(held_by_a_cleanup);
                        })
                        .unwrap();
                        reenter
                            .call(move |b| {
                                Box::pin(async move {
                                    let held_by_b_cleanup = b.clone();
                                    rt::defer_local_cleanup(async move {
                                        b_cleaning.send(()).unwrap();
                                        gate_b.await.unwrap();
                                        drop(held_by_b_cleanup);
                                    })
                                    .unwrap();
                                    let _last_self_proxy = last;
                                    entered.send(()).unwrap();
                                    // Fix the review's legal ordering: outer Drop releases its
                                    // proxy before either owner method resumes cancellation.
                                    owner_gate.recv().unwrap();
                                    pending::<()>().await;
                                })
                            })
                            .await
                    })
                })
                .await
        }));
        assert!(poll!(&mut outer).is_pending());
        ready.await.unwrap();
        context.cancel();
        assert!(poll!(&mut outer).is_pending());
        resume_owner.send(()).unwrap();
        a_started.await.unwrap();
        b_started.await.unwrap();
        let mut final_started = Box::pin(final_started);
        assert!(poll!(&mut final_started).is_pending());
        assert!(poll!(&mut outer).is_pending());
        release_b.send(()).unwrap();
        release_a.send(()).unwrap();
        // Both method cleanups released real Rc<T> references. A nested-release
        // cycle prevents registry destruction even though those references left.
        tokio::time::timeout(Duration::from_secs(2), &mut final_started)
            .await
            .expect("nested cancellation deadlocked before object destruction")
            .unwrap();
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        assert!(poll!(&mut outer).is_pending());
        release_final.send(()).unwrap();
        assert_eq!(outer.await, Err(rt::Cancelled));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        executor.close().await.unwrap();
    });
}

#[test]
fn review_p1_spawn_from_owner_keeps_an_independent_release_root() {
    block_on(async {
        for spawn_locally in [false, true] {
            let executor = LocalExecutor::new(1).unwrap();
            let (cleaning, started) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let cleaned = Arc::new(AtomicUsize::new(0));
            let copied = cleaned.clone();
            let object = executor
                .create(move || async move {
                    CloseProbe {
                        cleaning: Some(cleaning),
                        release: Some(gate),
                        cleaned: copied,
                        local: Rc::new(()),
                    }
                })
                .await
                .unwrap();
            let (entered, ready) = oneshot::channel();
            let spawn_owner = executor.clone();
            // The outer owner task completes and returns the independent child.
            // Its release root must not own that child's last-proxy confirmation.
            let child = executor
                .spawn(move || async move {
                    let work = async move {
                        let _last_proxy = object;
                        entered.send(()).unwrap();
                        pending::<()>().await;
                    };
                    if spawn_locally {
                        rt::spawn_local(work).unwrap()
                    } else {
                        spawn_owner.spawn(move || work).await.unwrap()
                    }
                })
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
            ready.await.unwrap();
            let mut cancelled = Box::pin(child.cancel_and_wait());
            assert!(poll!(&mut cancelled).is_pending());
            started.await.unwrap();
            assert!(poll!(&mut cancelled).is_pending());
            assert_eq!(cleaned.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            assert_eq!(cancelled.await, Err(TaskError::Cancelled));
            assert_eq!(cleaned.load(Ordering::SeqCst), 1);
            executor.close().await.unwrap();
        }
    });
}

#[test]
fn review_p1_dependent_create_cancel_shares_parent_release_confirmation() {
    block_on(async {
        let executor = LocalExecutor::new(1).unwrap();
        let (cleaning, started) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let copied = cleaned.clone();
        let object = executor
            .create(move || async move {
                CloseProbe {
                    cleaning: Some(cleaning),
                    release: Some(gate),
                    cleaned: copied,
                    local: Rc::new(()),
                }
            })
            .await
            .unwrap();
        let last = object.clone();
        let nested_owner = executor.clone();
        let (entered, ready) = oneshot::channel();
        let (resume, owner_gate) = std::sync::mpsc::channel();
        let context = CallContext::new(CancellationToken::new());
        let mut outer = Box::pin(context.run(async move {
            object
                .call(move |_| {
                    Box::pin(async move {
                        nested_owner
                            .create(move || async move {
                                let _last_self_proxy = last;
                                entered.send(()).unwrap();
                                owner_gate.recv().unwrap();
                                pending::<()>().await;
                            })
                            .await
                    })
                })
                .await
        }));
        assert!(poll!(&mut outer).is_pending());
        ready.await.unwrap();
        context.cancel();
        assert!(poll!(&mut outer).is_pending());
        resume.send(()).unwrap();
        started.await.unwrap();
        assert!(poll!(&mut outer).is_pending());
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert!(matches!(outer.await, Err(rt::Cancelled)));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        executor.close().await.unwrap();
    });
}
