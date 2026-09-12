use super::*;
use futures_util::{future::Shared, FutureExt};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
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
    pub launched: bool,
    pub managed: Vec<Managed>,
}
#[derive(Clone, Copy)]
pub(super) struct Status {
    pub mode: Mode,
    pub failure: Option<ActorError>,
}
impl Status {
    fn encode(self) -> u8 {
        let mode = match self.mode {
            Mode::Running => 0,
            Mode::Draining => 1,
            Mode::Closing => 2,
        };
        let failure = match self.failure {
            None => 0,
            Some(ActorError::InvalidCapacity) => 1,
            Some(ActorError::Closed) => 2,
            Some(ActorError::Cancelled) => 3,
            Some(ActorError::Panicked) => 4,
            Some(ActorError::ExecutorStopped) => 5,
            Some(ActorError::StartFailed) => 6,
        };
        mode | (failure << 2)
    }
    fn decode(value: u8) -> Self {
        let mode = match value & 3 {
            0 => Mode::Running,
            1 => Mode::Draining,
            2 => Mode::Closing,
            _ => unreachable!("invalid actor mode"),
        };
        let failure = match value >> 2 {
            0 => None,
            1 => Some(ActorError::InvalidCapacity),
            2 => Some(ActorError::Closed),
            3 => Some(ActorError::Cancelled),
            4 => Some(ActorError::Panicked),
            5 => Some(ActorError::ExecutorStopped),
            6 => Some(ActorError::StartFailed),
            _ => unreachable!("invalid actor failure"),
        };
        Self { mode, failure }
    }
}
// Keep Arc's refcounts off the hot control payload's cache line. This adds
// padding once per actor, not to each request.
#[cfg_attr(any(target_arch = "aarch64", target_arch = "x86_64"), repr(align(128)))]
pub(super) struct Control {
    pub state: Mutex<State>,
    pub status: AtomicU8,
    pub changed: Notify,
    pub slots: Arc<Semaphore>,
    pub stopping: CancellationToken,
    pub cancelling: CancellationToken,
    pub done: Signal,
}
impl Control {
    pub fn status(&self) -> Status {
        Status::decode(self.status.load(Ordering::Acquire))
    }
    // All writers hold state, preserving admission and final-drain ordering.
    pub fn set_status(&self, status: Status) {
        self.status.store(status.encode(), Ordering::Release);
    }
    pub fn stop(&self, mode: Mode) -> bool {
        let state = self.state.lock();
        let mut status = self.status();
        status.mode = status.mode.max(mode);
        self.set_status(status);
        self.stopping.cancel();
        if status.mode == Mode::Closing {
            self.cancelling.cancel();
        }
        for task in &state.managed {
            if task.timer || status.mode == Mode::Closing {
                task.token.cancel();
            }
        }
        let launched = state.launched;
        drop(state);
        self.changed.notify_one();
        launched
    }
    pub fn fail(&self, error: ActorError) {
        {
            let _state = self.state.lock();
            let mut status = self.status();
            if status.failure.is_none() {
                status.failure = Some(error);
                self.set_status(status);
            }
        }
        self.stop(Mode::Closing);
    }
    pub fn managed_count(&self) -> usize {
        self.state.lock().managed.len()
    }
}
