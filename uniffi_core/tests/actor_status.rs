#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]

use futures_channel::oneshot;
use futures_util::poll;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use uniffi_core::async_runtime::{actor::*, current_context, BoxFuture};

struct Probe {
    queued_runs: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    cleaning: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}
impl Actor for Probe {}
impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        let cleaning = self.cleaning.take().unwrap();
        let release = self.release.take().unwrap();
        current_context()
            .unwrap()
            .defer_cleanup(async move {
                cleaning.send(()).unwrap();
                release.await.unwrap();
            })
            .unwrap();
    }
}

struct Fail {
    started: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    first: ActorError,
    later: ActorError,
}
impl Message for Fail {
    type Reply = u32;
}
impl Handler<Fail> for Probe {
    type Future<'a> = BoxFuture<'a, u32>;
    fn handle<'a>(&'a mut self, message: Fail, context: ActorContext) -> Self::Future<'a> {
        Box::pin(async move {
            message.started.send(()).unwrap();
            message.release.await.unwrap();
            context.fail(message.first);
            context.fail(message.later);
            7
        })
    }
}
struct Queued;
impl Message for Queued {
    type Reply = u32;
}
impl Handler<Queued> for Probe {
    type Future<'a> = std::future::Ready<u32>;
    fn handle<'a>(&'a mut self, _: Queued, _: ActorContext) -> Self::Future<'a> {
        self.queued_runs.fetch_add(1, Ordering::SeqCst);
        std::future::ready(11)
    }
}

#[test]
fn first_failure_survives_drain_upgrade_later_failure_and_actor_cleanup() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                for (first, later) in [
                    (ActorError::StartFailed, ActorError::Panicked),
                    (ActorError::Panicked, ActorError::ExecutorStopped),
                ] {
                    let runs = Arc::new(AtomicUsize::new(0));
                    let dropped = Arc::new(AtomicUsize::new(0));
                    let (cleaning, clean_started) = oneshot::channel();
                    let (release_cleanup, cleanup_gate) = oneshot::channel();
                    let (owner, address) = start(
                        Probe {
                            queued_runs: runs.clone(),
                            dropped: dropped.clone(),
                            cleaning: Some(cleaning),
                            release: Some(cleanup_gate),
                        },
                        1,
                    )
                    .unwrap();
                    let (started, active) = oneshot::channel();
                    let (release, gate) = oneshot::channel();
                    let mut current = address.ask(Fail {
                        started,
                        release: gate,
                        first,
                        later,
                    });
                    assert!(poll!(&mut current).is_pending());
                    active.await.unwrap();
                    let mut queued = address.ask(Queued);
                    assert!(poll!(&mut queued).is_pending());
                    let mut drain = std::pin::pin!(owner.drain_and_close());
                    assert!(poll!(&mut drain).is_pending());
                    release.send(()).unwrap();
                    assert_eq!(current.await, Err(first));
                    assert_eq!(queued.await, Err(first));
                    clean_started.await.unwrap();
                    assert_eq!(runs.load(Ordering::SeqCst), 0);
                    assert_eq!(dropped.load(Ordering::SeqCst), 1);
                    assert!(poll!(&mut drain).is_pending());
                    release_cleanup.send(()).unwrap();
                    assert_eq!(drain.await, Err(first));
                    assert_eq!(owner.wait().await, Err(first));
                }
            })
            .await
            .unwrap();
        });
}
