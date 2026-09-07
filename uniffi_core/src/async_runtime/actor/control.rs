use super::*;
use futures_util::{future::Shared, FutureExt};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

pub(super) type Signal = Shared<oneshot::Receiver<Result<(), ActorError>>>;
pub(super) fn completion() -> (Finished, Signal) {
    let (sender, receiver) = oneshot::channel();
    (Finished(Some(sender)), receiver.shared())
}
/// Only the owner of the work owns the sender. Executor loss closes the signal.
pub(super) struct Finished(Option<oneshot::Sender<Result<(), ActorError>>>);
impl Finished {
    pub fn finish(mut self, result: Result<(), ActorError>) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(result);
        }
    }
}
pub(super) async fn wait(signal: Signal) -> Result<(), ActorError> {
    signal.await.unwrap_or(Err(ActorError::ExecutorStopped))
}
pub(super) fn defer(signal: Signal) {
    if let Some(context) = super::super::current_context() {
        context
            .defer_cleanup(async move {
                match wait(signal).await {
                    Ok(()) | Err(ActorError::Cancelled) => {}
                    Err(error) => panic!("actor cleanup failed: {error}"),
                }
            })
            .expect("current context accepts actor completion");
    }
}
pub(super) struct WaitGuard(pub Option<Signal>);
impl Drop for WaitGuard {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            defer(signal);
        }
    }
}
pub(super) async fn wait_owned(signal: Signal) -> Result<(), ActorError> {
    let mut guard = WaitGuard(Some(signal.clone()));
    let result = wait(signal).await;
    guard.0 = None;
    result
}
// Rejected admission cleans caller-owned payloads before returning. If that
// cleanup wait is itself dropped, transfer its driver to the enclosing S01 call.
pub(super) struct CleanupGuard(pub Option<CallContext>);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let (Some(cleanup), Some(parent)) = (self.0.take(), super::super::current_context()) {
            parent
                .defer_cleanup(async move {
                    cleanup
                        .wait_closed()
                        .await
                        .expect("actor payload cleanup failed");
                })
                .expect("current context accepts actor payload cleanup");
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Mode {
    Running,
    Draining,
    Closing,
}
pub(super) struct Managed {
    pub token: CancellationToken,
    pub signal: Signal,
    pub timer: bool,
}
pub(super) struct State {
    pub mode: Mode,
    pub launched: bool,
    pub failure: Option<ActorError>,
    pub managed: Vec<Managed>,
}
pub(super) struct Control {
    pub state: Mutex<State>,
    pub changed: Notify,
    pub slots: Arc<Semaphore>,
    pub stopping: CancellationToken,
    pub cancelling: CancellationToken,
    pub done: Signal,
}
impl Control {
    pub fn stop(&self, mode: Mode) -> bool {
        let mut state = self.state.lock().unwrap();
        state.mode = state.mode.max(mode);
        self.stopping.cancel();
        if state.mode == Mode::Closing {
            self.cancelling.cancel();
        }
        for task in &state.managed {
            if task.timer || state.mode == Mode::Closing {
                task.token.cancel();
            }
        }
        let launched = state.launched;
        drop(state);
        self.changed.notify_one();
        launched
    }
    pub fn fail(&self, error: ActorError) {
        self.state.lock().unwrap().failure.get_or_insert(error);
        self.stop(Mode::Closing);
    }
    pub fn managed_count(&self) -> usize {
        self.state.lock().unwrap().managed.len()
    }
}
