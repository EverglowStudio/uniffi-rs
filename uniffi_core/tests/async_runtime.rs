#![cfg(feature = "async-runtime")]

use uniffi_core::async_runtime::*;

#[test]
fn cancellation_token_keeps_send_and_sync_on_all_targets() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<CancellationToken>();
}

// These compile on the real wasm32 target, where Rc/RefCell futures are local.
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
fn wasm_local_bounds() {
    use std::{cell::RefCell, rc::Rc};
    let local = Rc::new(RefCell::new(1));
    let context = CallContext::new(CancellationToken::new());
    let cleanup_local = local.clone();
    context
        .defer_cleanup(async move {
            *cleanup_local.borrow_mut() += 1;
        })
        .unwrap();
    let _future: BoxFuture<'static, Rc<RefCell<i32>>> = Box::pin(async move {
        sleep(std::time::Duration::ZERO).await;
        local
    });
    let _stream: BoxStream<'static, Rc<RefCell<i32>>> =
        Box::pin(futures::stream::iter([Rc::new(RefCell::new(3))]));
    let local = Rc::new(RefCell::new(2));
    let _task = spawn(async move {
        sleep(std::time::Duration::ZERO).await;
        local
    });
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use futures::{
        channel::oneshot,
        executor::block_on,
        future::{pending, poll_fn},
        task::noop_waker,
    };
    use std::{
        cell::Cell,
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
        time::Duration,
    };

    fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        Pin::new(future).poll(&mut Context::from_waker(&noop_waker()))
    }
    struct DropCleanup {
        drops: Arc<AtomicUsize>,
        release: Option<oneshot::Receiver<()>>,
        cleaned: Arc<AtomicUsize>,
    }
    impl Drop for DropCleanup {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            let context = current_context().expect("drop has context");
            let release = self.release.take().unwrap();
            let cleaned = self.cleaned.clone();
            context
                .defer_cleanup(async move {
                    assert!(current_context().is_some());
                    let _ = release.await;
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
    #[test]
    fn cancel_before_first_poll_drops_without_polling_and_waits_cleanup() {
        let token = CancellationToken::new();
        let context = CallContext::new(token.clone());
        let (release, receiver) = oneshot::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let guard = DropCleanup {
            drops: drops.clone(),
            release: Some(receiver),
            cleaned: cleaned.clone(),
        };
        let mut call = Box::pin(context.run(async move {
            let _guard = guard;
            panic!("must not poll");
        }));
        token.cancel();
        token.cancel();
        assert!(poll_once(&mut call).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        assert!(current_context().is_none());
        release.send(()).unwrap();
        assert_eq!(block_on(call), Err(Cancelled));
        block_on(async {
            let (a, b) = futures::join!(context.wait_closed(), context.wait_closed());
            assert_eq!((a, b), (Ok(()), Ok(())));
        });
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(context.defer_cleanup(async {}), Err(CleanupClosed));
    }
    #[test]
    fn outer_drop_preserves_cleanup_and_cancelled_waiter_can_resume() {
        let context = CallContext::new(CancellationToken::new());
        let (release, receiver) = oneshot::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let guard = DropCleanup {
            drops: drops.clone(),
            release: Some(receiver),
            cleaned: cleaned.clone(),
        };
        let mut call = Box::pin(context.run(async move {
            let _guard = guard;
            pending::<()>().await;
        }));
        assert!(poll_once(&mut call).is_pending());
        drop(call);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut waiting = Box::pin(context.wait_closed());
        assert!(poll_once(&mut waiting).is_pending());
        drop(waiting);
        release.send(()).unwrap();
        assert_eq!(block_on(context.wait_closed()), Ok(()));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn cleanup_registration_during_cleanup_and_concurrent_dependencies() {
        let context = CallContext::new(CancellationToken::new());
        let (sender, receiver) = oneshot::channel();
        context
            .defer_cleanup(async move {
                receiver.await.unwrap();
            })
            .unwrap();
        context
            .defer_cleanup(async move {
                current_context().unwrap().defer_cleanup(async {}).unwrap();
                sender.send(()).unwrap();
            })
            .unwrap();
        assert_eq!(block_on(context.run(async { 17 })), Ok(17));
    }
    #[test]
    fn complete_commit_wins_late_cancel_during_cleanup() {
        let token = CancellationToken::new();
        let context = CallContext::new(token.clone());
        let (sender, receiver) = oneshot::channel();
        context
            .defer_cleanup(async move {
                receiver.await.unwrap();
            })
            .unwrap();
        let mut call = Box::pin(context.run(async { 23 }));
        assert!(poll_once(&mut call).is_pending());
        token.cancel();
        sender.send(()).unwrap();
        assert_eq!(block_on(call), Ok(23));
    }
    #[test]
    fn cancel_during_ready_poll_wins_and_never_repolls() {
        let token = CancellationToken::new();
        let context = CallContext::new(token.clone());
        let mut count = 0;
        let call = context.run(poll_fn(move |_| {
            count += 1;
            assert_eq!(count, 1);
            token.cancel();
            Poll::Ready(5)
        }));
        assert_eq!(block_on(call), Err(Cancelled));
    }
    #[test]
    fn nested_context_restored_after_poll_and_drop() {
        let outer = CallContext::new(CancellationToken::new());
        let inner = CallContext::new(CancellationToken::new());
        let token = outer.cancellation_token();
        assert_eq!(
            block_on(outer.run(async move {
                assert_eq!(
                    inner
                        .run(async {
                            current_context().unwrap().cancellation_token().cancel();
                        })
                        .await,
                    Err(Cancelled)
                );
                assert!(!current_context()
                    .unwrap()
                    .cancellation_token()
                    .is_cancelled());
                token.cancel();
            })),
            Err(Cancelled)
        );
        assert!(current_context().is_none());
    }
    #[test]
    fn many_cancel_waiters_and_threads_no_lost_notification() {
        let token = CancellationToken::new();
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let token = token.clone();
                std::thread::spawn(move || block_on(token.cancelled()))
            })
            .collect();
        token.cancel();
        for waiter in waiters {
            waiter.join().unwrap();
        }
        block_on(token.cancelled());
    }
    #[test]
    fn native_future_and_output_need_send_not_sync() {
        let future = async {
            let state = Cell::new(42);
            poll_fn(|_| Poll::Ready(())).await;
            state
        };
        fn assert_send<T: Send>(_: &T) {}
        let call = run(future);
        assert_send(&call);
        assert_eq!(block_on(call).unwrap().get(), 42);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(spawn(async { Cell::new(7) }).wait().await.unwrap().get(), 7);
        });
    }
    #[test]
    fn panic_waits_cleanup_and_restores_scope() {
        let context = CallContext::new(CancellationToken::new());
        let (sender, receiver) = oneshot::channel();
        context
            .defer_cleanup(async move {
                receiver.await.unwrap();
            })
            .unwrap();
        let mut call = Box::pin(context.run(async {
            panic!("business panic");
        }));
        assert!(poll_once(&mut call).is_pending());
        assert!(current_context().is_none());
        sender.send(()).unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on(call))).is_err());
        assert_eq!(block_on(context.wait_closed()), Ok(()));
    }
    #[test]
    fn one_cleanup_panic_does_not_skip_other_cleanup() {
        let context = CallContext::new(CancellationToken::new());
        let cleaned = Arc::new(AtomicUsize::new(0));
        context
            .defer_cleanup(async {
                panic!("cleanup panic");
            })
            .unwrap();
        let other = cleaned.clone();
        context
            .defer_cleanup(async move {
                other.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on(
                context.run(async {})
            )))
            .is_err()
        );
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(block_on(context.wait_closed()), Err(CleanupFailed));
    }
    #[test]
    fn business_destructor_panic_still_waits_registered_cleanup() {
        struct PanickingDrop(Option<oneshot::Receiver<()>>);
        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                let receiver = self.0.take().unwrap();
                current_context()
                    .unwrap()
                    .defer_cleanup(async move {
                        receiver.await.unwrap();
                    })
                    .unwrap();
                panic!("business destructor panic");
            }
        }
        let context = CallContext::new(CancellationToken::new());
        let (sender, receiver) = oneshot::channel();
        let guard = PanickingDrop(Some(receiver));
        let mut call = Box::pin(context.run(async move {
            let _guard = guard;
            pending::<()>().await;
        }));
        assert!(poll_once(&mut call).is_pending());
        context.cancel();
        assert!(poll_once(&mut call).is_pending());
        sender.send(()).unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on(call))).is_err());
        assert_eq!(block_on(context.wait_closed()), Ok(()));
    }
    #[test]
    fn cleanup_destructor_can_register_cleanup_without_lock_reentry() {
        struct RegisterOnDrop(Arc<AtomicUsize>);
        impl Drop for RegisterOnDrop {
            fn drop(&mut self) {
                let count = self.0.clone();
                current_context()
                    .unwrap()
                    .defer_cleanup(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                    })
                    .unwrap();
            }
        }
        let context = CallContext::new(CancellationToken::new());
        let count = Arc::new(AtomicUsize::new(0));
        let guard = RegisterOnDrop(count.clone());
        context
            .defer_cleanup(async move {
                drop(guard);
            })
            .unwrap();
        assert_eq!(block_on(context.run(async {})), Ok(()));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn concurrent_cleanup_registration_wakes_pending_driver() {
        let context = CallContext::new(CancellationToken::new());
        let (sender, receiver) = oneshot::channel();
        context
            .defer_cleanup(async move {
                receiver.await.unwrap();
            })
            .unwrap();
        let mut call = Box::pin(context.run(async { 31 }));
        assert!(poll_once(&mut call).is_pending());
        let closing = context.clone();
        let waiter = std::thread::spawn(move || block_on(closing.wait_closed()));
        context
            .defer_cleanup(async move {
                sender.send(()).unwrap();
            })
            .unwrap();
        assert_eq!(waiter.join().unwrap(), Ok(()));
        assert_eq!(block_on(call), Ok(31));
    }
    #[test]
    fn duplicate_run_is_rejected_without_reusing_the_context() {
        let context = CallContext::new(CancellationToken::new());
        let first = context.run(async {});
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| context.run(async {})))
                .is_err()
        );
        assert_eq!(block_on(first), Ok(()));
    }
    #[test]
    fn timers_zero_overflow_and_timeout_ready_priority() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            sleep(Duration::ZERO).await;
            assert_eq!(timeout(Duration::ZERO, async { 7 }).await, Ok(7));
            assert_eq!(timeout(Duration::ZERO, pending::<()>()).await, Err(Elapsed));
            let long = spawn(sleep(Duration::MAX));
            assert_eq!(long.cancel_and_wait().await, Err(TaskError::Cancelled));
        });
    }
}
