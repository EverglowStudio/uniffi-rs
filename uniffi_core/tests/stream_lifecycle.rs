#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]
use futures::{channel::oneshot, future::pending, poll, Stream, StreamExt};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};
use uniffi_core::{
    async_runtime::{self as rt, CallContext, CancellationToken},
    *,
};

struct UniFfiTag;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), future)
                .await
                .expect("stream lifecycle gate timed out")
        })
}
fn registry<T, E>() -> &'static RustStreamRegistry<T, E> {
    Box::leak(Box::new(once_cell::sync::Lazy::new(Default::default)))
}
#[derive(Default)]
struct Counts {
    polls: AtomicUsize,
    drops: AtomicUsize,
    cleaned: AtomicUsize,
}
struct Probe {
    counts: Arc<Counts>,
    gate: Option<oneshot::Receiver<()>>,
    outcome: Option<Option<Result<Option<u32>, String>>>,
}
impl Stream for Probe {
    type Item = Result<Option<u32>, String>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.counts.polls.fetch_add(1, Ordering::SeqCst);
        assert!(rt::current_context().is_some());
        match self.outcome.take() {
            Some(value) => Poll::Ready(value),
            None => Poll::Pending,
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.gate.take() {
            let counts = self.counts.clone();
            rt::current_context()
                .expect("subscription drop scope")
                .defer_cleanup(async move {
                    gate.await.unwrap();
                    counts.cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
}
fn probe(
    outcome: Option<Option<Result<Option<u32>, String>>>,
) -> (Probe, Arc<Counts>, oneshot::Sender<()>) {
    let counts = Arc::new(Counts::default());
    let (release, gate) = oneshot::channel();
    (
        Probe {
            counts: counts.clone(),
            gate: Some(gate),
            outcome,
        },
        counts,
        release,
    )
}

#[test]
fn idle_close_drops_without_prefetch_and_repeated_waiters_share_cleanup() {
    block_on(async {
        let reg = registry();
        let (stream, counts, release) = probe(None);
        let handle = rust_stream_new(reg, Box::pin(stream));
        assert_eq!(counts.polls.load(Ordering::SeqCst), 0);
        let mut one = Box::pin(rust_stream_close_async(reg, handle.clone()));
        let mut two = Box::pin(rust_stream_close_async(reg, handle.clone()));
        assert!(poll!(&mut one).is_pending());
        assert!(poll!(&mut two).is_pending());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert_eq!(one.await, Ok(()));
        assert_eq!(two.await, Ok(()));
        assert_eq!(rust_stream_close_async(reg, handle).await, Ok(()));
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        assert!(reg.lock().unwrap().is_empty());
    });
}

#[test]
fn close_releases_pending_stream_without_repolling_the_retained_next_waiter() {
    block_on(async {
        let reg = registry();
        let (stream, counts, release) = probe(None);
        let handle = rust_stream_new(reg, Box::pin(stream));
        let mut next = Box::pin(rust_stream_next_async(reg, handle.clone()));
        assert!(poll!(&mut next).is_pending());
        let mut close = Box::pin(rust_stream_close_async(reg, handle));
        assert!(poll!(&mut close).is_pending());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        close.await.unwrap();
        assert_eq!(counts.polls.load(Ordering::SeqCst), 1);
        assert!(matches!(next.await, Err(error) if error.error.is::<StreamCancelled>()));
    });
}

#[test]
fn dropping_current_next_keeps_subscription_and_does_not_restart_business() {
    block_on(async {
        let reg = registry();
        let (stream, counts, release) = probe(Some(Some(Ok(None))));
        let handle = rust_stream_new(reg, Box::pin(stream));
        assert!(matches!(
            rust_stream_next_async(reg, handle.clone()).await,
            Ok(UniFfiStreamStep::Item(None))
        ));
        let mut first = Box::pin(rust_stream_next_async(reg, handle.clone()));
        assert!(poll!(&mut first).is_pending());
        let concurrent = rust_stream_next_async(reg, handle.clone()).await;
        assert!(concurrent.is_err());
        drop(first);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
        assert_eq!(counts.polls.load(Ordering::SeqCst), 2);
        let mut again = Box::pin(rust_stream_next_async(reg, handle.clone()));
        assert!(poll!(&mut again).is_pending());
        assert_eq!(counts.polls.load(Ordering::SeqCst), 3);
        release.send(()).unwrap();
        rust_stream_close_async(reg, handle).await.unwrap();
        drop(again);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn first_error_and_eof_are_terminal_and_wait_for_drop_cleanup() {
    for outcome in [None, Some(Err("first".into()))] {
        block_on(async {
            let error = outcome.is_some();
            let reg = registry();
            let (stream, counts, release) = probe(Some(outcome));
            let handle = rust_stream_new(reg, Box::pin(stream));
            let mut next = Box::pin(rust_stream_next_async(reg, handle.clone()));
            assert!(poll!(&mut next).is_pending());
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
            assert_eq!(counts.polls.load(Ordering::SeqCst), 1);
            release.send(()).unwrap();
            let step = next.await.unwrap();
            assert_eq!(
                step,
                if error {
                    UniFfiStreamStep::Error("first".into())
                } else {
                    UniFfiStreamStep::Done
                }
            );
            assert_eq!(
                rust_stream_next_async(reg, handle).await.unwrap(),
                UniFfiStreamStep::Done
            );
            assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        });
    }
}

#[test]
fn cancelling_or_dropping_started_close_transfers_confirmation_to_parent() {
    for cancel in [false, true] {
        block_on(async {
            let reg = registry();
            let (stream, counts, release) = probe(None);
            let handle = rust_stream_new(reg, Box::pin(stream));
            let parent = CallContext::new(CancellationToken::new());
            let mut run =
                Box::pin(parent.run(async move { rust_stream_close_async(reg, handle).await }));
            assert!(poll!(&mut run).is_pending());
            if cancel {
                parent.cancel();
                assert!(poll!(&mut run).is_pending());
            }
            drop(run);
            let mut wait = Box::pin(parent.wait_closed());
            assert!(poll!(&mut wait).is_pending());
            assert_eq!(counts.cleaned.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            wait.await.unwrap();
            assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
            assert!(reg.lock().unwrap().is_empty());
        });
    }
}

#[test]
fn synchronous_cancel_hands_cleanup_to_the_owning_call() {
    block_on(async {
        let reg = registry();
        let (stream, counts, release) = probe(None);
        let handle = rust_stream_new(reg, Box::pin(stream));
        let parent = CallContext::new(CancellationToken::new());
        let mut run = Box::pin(parent.run(async move {
            rust_stream_cancel(reg, handle);
        }));
        assert!(poll!(&mut run).is_pending());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        run.await.unwrap();
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        assert!(reg.lock().unwrap().is_empty());
    });
}

struct LocalProbe {
    owner: std::thread::ThreadId,
    rc: std::rc::Rc<()>,
    probe: Probe,
}
impl Stream for LocalProbe {
    type Item = Result<Option<u32>, String>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        assert_eq!(std::thread::current().id(), self.owner);
        Pin::new(&mut self.probe).poll_next(cx)
    }
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
        let gate = self.probe.gate.take().unwrap();
        let counts = self.probe.counts.clone();
        let rc = self.rc.clone();
        let owner = self.owner;
        rt::defer_local_cleanup(async move {
            gate.await.unwrap();
            assert_eq!(std::thread::current().id(), owner);
            drop(rc);
            counts.cleaned.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    }
}
async fn local_probe(
    executor: &rt::LocalExecutor,
) -> (
    UniFfiStream<Option<u32>, String>,
    Arc<Counts>,
    oneshot::Sender<()>,
) {
    let (probe, counts, release) = probe(Some(Some(Ok(None))));
    let stream = executor
        .spawn(move || async move {
            rt::local_stream(LocalProbe {
                owner: std::thread::current().id(),
                rc: std::rc::Rc::new(()),
                probe,
            })
            .unwrap()
        })
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&stream);
    (stream, counts, release)
}

#[test]
fn local_stream_constructs_polls_drops_and_cleans_on_owner_without_prefetch() {
    block_on(async {
        assert!(matches!(
            rt::local_stream(futures::stream::pending::<Result<u32, String>>()),
            Err(rt::LocalError::NoOwner)
        ));
        let executor = rt::LocalExecutor::new(1).unwrap();
        let (stream, counts, release) = local_probe(&executor).await;
        assert_eq!(counts.polls.load(Ordering::SeqCst), 0);
        let reg = registry();
        let handle = rust_stream_new(reg, stream);
        assert_eq!(
            rust_stream_next_async(reg, handle.clone()).await.unwrap(),
            UniFfiStreamStep::Item(None)
        );
        assert_eq!(counts.polls.load(Ordering::SeqCst), 1);
        let mut close = Box::pin(rust_stream_close_async(reg, handle));
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        executor.close().await.unwrap();
    });
}

#[test]
fn undelivered_local_stream_factory_output_drop_waits_for_owner_cleanup() {
    block_on(async {
        let executor = rt::LocalExecutor::new(1).unwrap();
        let (stream, counts, release) = local_probe(&executor).await;
        let parent = CallContext::new(CancellationToken::new());
        let token = parent.cancellation_token();
        let output = RustStreamOutput::new(registry(), stream);
        let mut run = Box::pin(parent.run(async move {
            token.cancel();
            output
        }));
        assert!(poll!(&mut run).is_pending());
        assert_eq!(counts.polls.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert!(run.await.is_err());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        executor.close().await.unwrap();
    });
}

#[derive(Clone)]
struct HostOps {
    closes: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    gate: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    cleaned: Arc<AtomicUsize>,
}
impl ForeignInputStreamOps<u32, String> for HostOps {
    fn next(&self, _: Handle) -> ForeignInputStreamNextFuture<u32, String> {
        Box::pin(pending())
    }
    fn cancel(&self, _: Handle) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }
    fn close(&self, _: Handle) -> ForeignInputStreamCloseFuture {
        self.closes.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().take().unwrap();
        let cleaned = self.cleaned.clone();
        Box::pin(async move {
            gate.await.unwrap();
            cleaned.fetch_add(1, Ordering::SeqCst);
        })
    }
}
fn input() -> (UniFfiInputStream<u32, String>, HostOps, oneshot::Sender<()>) {
    let (release, gate) = oneshot::channel();
    let ops = HostOps {
        closes: Arc::default(),
        cancels: Arc::default(),
        cleaned: Arc::default(),
        gate: Arc::new(Mutex::new(Some(gate))),
    };
    (
        UniFfiInputStream::new(Handle::from_raw_unchecked(1), ops.clone()),
        ops,
        release,
    )
}
#[test]
fn input_drop_waits_for_host_acknowledgement_not_sync_cancel_request() {
    block_on(async {
        let (mut stream, ops, release) = input();
        let context = CallContext::new(CancellationToken::new());
        let mut run = Box::pin(context.run(async move { stream.next().await }));
        assert!(poll!(&mut run).is_pending());
        context.cancel();
        assert!(poll!(&mut run).is_pending());
        assert_eq!(ops.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(ops.closes.load(Ordering::SeqCst), 1);
        assert_eq!(ops.cleaned.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        assert!(run.await.is_err());
        assert_eq!(ops.cleaned.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn dropped_input_close_waiter_and_repeated_close_share_host_acknowledgement() {
    block_on(async {
        let (mut stream, ops, release) = input();
        let parent = CallContext::new(CancellationToken::new());
        let mut run = Box::pin(parent.run(stream.close()));
        assert!(poll!(&mut run).is_pending());
        drop(run);
        let mut wait = Box::pin(parent.wait_closed());
        assert!(poll!(&mut wait).is_pending());
        release.send(()).unwrap();
        wait.await.unwrap();
        stream.close().await.unwrap();
        assert_eq!(ops.closes.load(Ordering::SeqCst), 1);
        assert_eq!(ops.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(ops.cleaned.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn infallible_metadata_and_callback_result_traits_are_available() {
    use std::convert::Infallible;
    type S = UniFfiStream<u32, Infallible>;
    let meta = <S as TypeId<UniFfiTag>>::TYPE_ID_META;
    assert_eq!(
        &meta.bytes[..meta.size],
        &[
            metadata::codes::TYPE_STREAM,
            metadata::codes::TYPE_U32,
            metadata::codes::TYPE_UNIT,
            1
        ]
    );
    fn accepts<T: LiftReturn<UniFfiTag>>() {}
    accepts::<Result<Option<u32>, Infallible>>();
    assert!(matches!(
        <UniFfiStreamStep<u32, Infallible> as LowerReturn<UniFfiTag>>::handle_failed_lift(
            LiftArgsError {
                arg_name: "stream",
                error: StreamCancelled.into()
            }
        ),
        Err(RustCallError::Cancelled)
    ));
    assert!(<Infallible as Lift<UniFfiTag>>::try_read(&mut &[][..]).is_err());
}

#[test]
fn subscription_cleanup_can_reenter_cancel_without_waiting_on_itself() {
    struct CleanupCancel {
        registry: &'static RustStreamRegistry<u32, String>,
        handle: Arc<Mutex<Option<Handle>>>,
        cleaned: Arc<AtomicUsize>,
    }
    impl Stream for CleanupCancel {
        type Item = Result<u32, String>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for CleanupCancel {
        fn drop(&mut self) {
            let handle = self.handle.lock().unwrap().clone().unwrap();
            let registry = self.registry;
            let cleaned = self.cleaned.clone();
            rt::current_context()
                .unwrap()
                .defer_cleanup(async move {
                    rust_stream_cancel(registry, handle);
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
    block_on(async {
        let registry = registry();
        let slot = Arc::new(Mutex::new(None));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let stream = CleanupCancel {
            registry,
            handle: slot.clone(),
            cleaned: cleaned.clone(),
        };
        let handle = rust_stream_new(registry, Box::pin(stream));
        *slot.lock().unwrap() = Some(handle.clone());
        rust_stream_close_async(registry, handle).await.unwrap();
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn local_pending_next_close_and_cancelled_close_wait_for_owner_release() {
    block_on(async {
        let executor = rt::LocalExecutor::new(1).unwrap();
        let (stream, counts, release) = local_probe(&executor).await;
        let reg = registry();
        let handle = rust_stream_new(reg, stream);
        assert!(matches!(
            rust_stream_next_async(reg, handle.clone()).await,
            Ok(UniFfiStreamStep::Item(None))
        ));
        let mut next = Box::pin(rust_stream_next_async(reg, handle.clone()));
        assert!(poll!(&mut next).is_pending());
        let context = CallContext::new(CancellationToken::new());
        let mut close = Box::pin(context.run(rust_stream_close_async(reg, handle)));
        assert!(poll!(&mut close).is_pending());
        context.cancel();
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        assert!(close.await.is_err());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        assert!(matches!(next.await, Err(error) if error.error.is::<StreamCancelled>()));
        executor.close().await.unwrap();
    });
}

#[test]
fn local_method_returns_only_the_send_proxy() {
    block_on(async {
        let executor = rt::LocalExecutor::new(1).unwrap();
        let object = executor
            .create(|| async { std::rc::Rc::new(7u32) })
            .await
            .unwrap();
        let mut stream = object
            .call(|value| {
                Box::pin(async move {
                    let local = value.clone();
                    rt::local_stream(futures::stream::once(
                        async move { Ok::<_, String>(*local) },
                    ))
                    .unwrap()
                })
            })
            .await
            .unwrap();
        assert_eq!(stream.next().await, Some(Ok(7)));
        assert_eq!(stream.next().await, None);
        drop(stream);
        object.close().await.unwrap();
        executor.close().await.unwrap();
    });
}

#[test]
fn closing_local_owner_cancels_next_only_after_owner_cleanup() {
    for already_polled in [false, true] {
        block_on(async {
            let executor = rt::LocalExecutor::new(1).unwrap();
            let (stream, counts, release) = local_probe(&executor).await;
            let reg = registry();
            let handle = rust_stream_new(reg, stream);
            if already_polled {
                assert_eq!(
                    rust_stream_next_async(reg, handle.clone()).await.unwrap(),
                    UniFfiStreamStep::Item(None)
                );
            }
            let mut closing = Box::pin(executor.close());
            assert!(poll!(&mut closing).is_pending());
            while counts.drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            let mut next = Box::pin(rust_stream_next_async(reg, handle));
            assert!(
                poll!(&mut next).is_pending(),
                "reply closure must not acknowledge owner cleanup"
            );
            assert!(poll!(&mut closing).is_pending());
            assert_eq!(counts.cleaned.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            assert!(matches!(next.await, Err(error) if error.error.is::<StreamCancelled>()));
            closing.await.unwrap();
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
            assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
            assert!(reg.lock().unwrap().is_empty());
        });
    }
}

#[test]
fn local_poll_registered_cleanup_belongs_to_subscription_not_each_next() {
    struct PollCleanup {
        gate: Option<oneshot::Receiver<()>>,
        cleanups: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Stream for PollCleanup {
        type Item = Result<u32, String>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if let Some(gate) = self.gate.take() {
                let cleanups = self.cleanups.clone();
                let dropped = self.dropped.clone();
                let local = std::rc::Rc::new(());
                rt::defer_local_cleanup(async move {
                    assert_eq!(dropped.load(Ordering::SeqCst), 1);
                    gate.await.unwrap();
                    drop(local);
                    cleanups.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
            }
            Poll::Ready(Some(Ok(1)))
        }
    }
    impl Drop for PollCleanup {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    block_on(async {
        let owner = rt::LocalExecutor::new(1).unwrap();
        let (release, gate) = oneshot::channel();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let stream = PollCleanup {
            gate: Some(gate),
            cleanups: cleaned.clone(),
            dropped: dropped.clone(),
        };
        let stream = owner
            .spawn(move || async move { rt::local_stream(stream).unwrap() })
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let reg = registry();
        let handle = rust_stream_new(reg, stream);
        assert_eq!(
            rust_stream_next_async(reg, handle.clone()).await.unwrap(),
            UniFfiStreamStep::Item(1)
        );
        assert_eq!(
            rust_stream_next_async(reg, handle.clone()).await.unwrap(),
            UniFfiStreamStep::Item(1)
        );
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        let mut close = Box::pin(rust_stream_close_async(reg, handle));
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        owner.close().await.unwrap();
    });
}

#[test]
fn cancellation_of_produced_uncommitted_item_waits_for_its_cleanup() {
    struct Item(Option<oneshot::Receiver<()>>, Arc<AtomicUsize>);
    impl Drop for Item {
        fn drop(&mut self) {
            let gate = self.0.take().unwrap();
            let cleaned = self.1.clone();
            rt::current_context()
                .expect("discarded item drop context")
                .defer_cleanup(async move {
                    gate.await.unwrap();
                    cleaned.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
    struct Producer {
        item: Option<Item>,
        reg: &'static RustStreamRegistry<Item, String>,
        handle: Arc<Mutex<Option<Handle>>>,
    }
    impl Stream for Producer {
        type Item = Result<Item, String>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let handle = self.handle.lock().unwrap().clone().unwrap();
            rust_stream_cancel(self.reg, handle);
            Poll::Ready(Some(Ok(self.item.take().unwrap())))
        }
    }
    block_on(async {
        let reg = registry();
        let (release, gate) = oneshot::channel();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let slot = Arc::new(Mutex::new(None));
        let handle = rust_stream_new(
            reg,
            Box::pin(Producer {
                item: Some(Item(Some(gate), cleaned.clone())),
                reg,
                handle: slot.clone(),
            }),
        );
        *slot.lock().unwrap() = Some(handle.clone());
        let mut next = Box::pin(rust_stream_next_async(reg, handle.clone()));
        assert!(poll!(&mut next).is_pending());
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        let mut close = Box::pin(rust_stream_close_async(reg, handle));
        assert!(poll!(&mut close).is_pending());
        release.send(()).unwrap();
        close.await.unwrap();
        assert!(matches!(next.await, Err(error) if error.error.is::<StreamCancelled>()));
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn terminal_drop_panic_does_not_skip_registered_cleanup() {
    use futures::FutureExt;
    struct PanicDrop {
        _probe: Probe,
    }
    impl Stream for PanicDrop {
        type Item = Result<Option<u32>, String>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic!("stream destructor failure");
        }
    }
    block_on(async {
        let reg = registry();
        let (stream, counts, release) = probe(None);
        let handle = rust_stream_new(reg, Box::pin(PanicDrop { _probe: stream }));
        let mut next = Box::pin(
            std::panic::AssertUnwindSafe(rust_stream_next_async(reg, handle)).catch_unwind(),
        );
        assert!(poll!(&mut next).is_pending());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        assert!(next.await.is_err());
        assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
        assert!(reg.lock().unwrap().is_empty());
    });
}

// R1-03: use the existing Ready-path RawWaker drop as the scheduling gate.
// No test hook or alternate state transition is inserted into production code.
mod review_p1_ready_handoff {
    use super::*;
    use std::{
        sync::mpsc,
        task::{RawWaker, RawWakerVTable, Waker},
        thread,
    };

    struct Gate {
        clones: AtomicUsize,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    struct WakerData {
        gate: Arc<Gate>,
        pause_on_drop: bool,
    }

    fn raw_waker(data: WakerData) -> RawWaker {
        RawWaker::new(Box::into_raw(Box::new(data)).cast(), &VTABLE)
    }

    unsafe fn clone_waker(pointer: *const ()) -> RawWaker {
        // SAFETY: every vtable pointer owns a live Box<WakerData>. Cloning
        // borrows it and gives the new RawWaker its own Box and Arc reference.
        let data = unsafe { &*pointer.cast::<WakerData>() };
        raw_waker(WakerData {
            gate: data.gate.clone(),
            // RustStreamNext's first clone is the pending_waker. Any later
            // clones used by Run's cancellation signal cannot trigger the gate.
            pause_on_drop: data.gate.clones.fetch_add(1, Ordering::SeqCst) == 0,
        })
    }

    unsafe fn wake(pointer: *const ()) {
        // SAFETY: wake consumes exactly this RawWaker's unique Box. This test
        // polls explicitly and needs no scheduler notification.
        drop(unsafe { Box::from_raw(pointer.cast_mut().cast::<WakerData>()) });
    }

    unsafe fn wake_by_ref(_: *const ()) {}

    unsafe fn drop_waker(pointer: *const ()) {
        // SAFETY: drop consumes exactly this RawWaker's unique Box. All shared
        // gate access uses Arc, atomics and Mutex, so Waker remains Send + Sync.
        let data = unsafe { Box::from_raw(pointer.cast_mut().cast::<WakerData>()) };
        if data.pause_on_drop {
            data.gate.entered.send(()).unwrap();
            data.gate
                .resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("Ready handoff gate was not released");
        }
    }

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

    #[test]
    fn review_p1_ready_overlap_cannot_observe_an_empty_open_stream() {
        block_on(async {
            let reg = registry::<u32, String>();
            let handle = rust_stream_new(reg, Box::pin(futures::stream::iter([Ok(1), Ok(2)])));
            let (entered, at_ready_handoff) = mpsc::channel();
            let (resume, wait_for_resume) = mpsc::channel();
            let gate = Arc::new(Gate {
                clones: AtomicUsize::new(0),
                entered,
                resume: Mutex::new(wait_for_resume),
            });
            let first_handle = handle.clone();
            let first = thread::spawn(move || {
                // SAFETY: raw_waker allocates the exact Box owned by this vtable;
                // clone, wake and drop above preserve its ownership contract.
                let waker = unsafe {
                    Waker::from_raw(raw_waker(WakerData {
                        gate,
                        pause_on_drop: false,
                    }))
                };
                let mut cx = Context::from_waker(&waker);
                let mut next = Box::pin(rust_stream_next_async(reg, first_handle));
                next.as_mut().poll(&mut cx)
            });
            at_ready_handoff
                .recv_timeout(Duration::from_secs(5))
                .expect("first next never reached its Ready-path waker drop");

            // A is paused after clearing the Ready waker, before PollLease::drop.
            // B is a distinct next Future on this test thread, never a concurrent
            // poll of A's Future. Polling B completes before A may return its lease.
            let mut second = Box::pin(rust_stream_next_async(reg, handle.clone()));
            let before_release = poll!(&mut second);
            eprintln!("R1-03 B before releasing A: {before_release:?}");
            resume.send(()).unwrap();
            let first_result = first.join().unwrap();
            let second_result = match before_release {
                Poll::Ready(result) => result,
                Poll::Pending => second.await,
            };
            let still_registered = reg.lock().unwrap().contains_key(&handle.as_raw());
            let subsequent = rust_stream_next_async(reg, handle.clone()).await;
            let final_step = rust_stream_next_async(reg, handle.clone()).await;
            rust_stream_close_async(reg, handle).await.unwrap();
            eprintln!(
                "R1-03 after releasing A: A={first_result:?}, B={second_result:?}, \
                 registered={still_registered}, subsequent={subsequent:?}, final={final_step:?}"
            );

            // Assert only after releasing and joining A and closing the registry,
            // so the red run cannot leave a gated thread or subscription behind.
            assert!(matches!(
                first_result,
                Poll::Ready(Ok(UniFfiStreamStep::Item(1)))
            ));
            assert!(
                matches!(&second_result, Err(error) if error.error.to_string() == "concurrent stream next() is not supported"),
                "overlapping B must be rejected until A returns its lease, got {second_result:?}"
            );
            assert!(
                still_registered,
                "overlapping B prematurely closed the subscription"
            );
            assert_eq!(subsequent.unwrap(), UniFfiStreamStep::Item(2));
            assert_eq!(final_step.unwrap(), UniFfiStreamStep::Done);
            assert!(reg.lock().unwrap().is_empty());
        });
    }
}
