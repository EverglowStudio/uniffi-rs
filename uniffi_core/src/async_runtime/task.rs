//! Tasks on the current native runtime or WASM event loop.

use std::{fmt, future::Future, panic::AssertUnwindSafe};

use futures_channel::oneshot;
use futures_util::FutureExt;

use super::{CallContext, CancellationToken, SendOutsideWasm};

/// A task's terminal failure, separate from any business error in its output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskError {
    /// Cancellation was accepted; the future and registered cleanup are done.
    Cancelled,
    /// The task or registered cleanup panicked on a target supporting unwinding.
    Panicked,
    /// The executor dropped the task before it could report completion.
    /// Asynchronous cleanup is not guaranteed to have completed in this case.
    ExecutorStopped,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "task cancelled",
            Self::Panicked => "task panicked",
            Self::ExecutorStopped => "task executor stopped",
        })
    }
}

impl std::error::Error for TaskError {}

/// A task's cancellation control and single result receiver.
///
/// Dropping this handle (or an in-progress wait) detaches the task. Keep the
/// handle and use [`Self::cancel_and_wait`] to confirm cooperative shutdown.
#[derive(Debug)]
pub struct TaskHandle<T> {
    pub(super) cancellation: CancellationToken,
    pub(super) result: oneshot::Receiver<Result<T, TaskError>>,
}

impl<T> TaskHandle<T> {
    /// Clone this task's cancellation signal for explicit propagation to other
    /// operations. Cancelling the returned token also cancels this task.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Request cancellation. Repeated requests are harmless; this does not wait.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Consume the handle and await the result and registered cleanup.
    pub async fn wait(self) -> Result<T, TaskError> {
        self.result.await.unwrap_or(Err(TaskError::ExecutorStopped))
    }

    /// Request cancellation and await the result and registered cleanup.
    /// A result committed before cancellation is preserved.
    pub async fn cancel_and_wait(self) -> Result<T, TaskError> {
        self.cancel();
        self.wait().await
    }
}

/// Spawn an independent task on the current executor.
///
/// Native futures and outputs must be `Send`; neither needs to be `Sync`. WASM
/// futures and outputs may hold local values such as `Rc` and `JsValue`. Spawn
/// does not inherit its caller's cancellation: the returned handle owns it.
/// Panics become [`TaskError::Panicked`] only when the target supports unwinding;
/// panic-abort targets cannot recover a panic.
///
/// # Panics
///
/// On native this panics outside a Tokio runtime. It does not create a runtime.
pub fn spawn<F>(future: F) -> TaskHandle<F::Output>
where
    F: Future + SendOutsideWasm + 'static,
    F::Output: SendOutsideWasm + 'static,
{
    let cancellation = CancellationToken::new();
    let context = CallContext::new(cancellation.clone());
    let (sender, result) = oneshot::channel();
    // Establish the drop scope before submitting: even an executor that drops
    // the task without polling it must drop business captures in their context.
    let run = context.run(future);
    let task = async move {
        let result = AssertUnwindSafe(run).catch_unwind().await;
        let cleanup = context.wait_closed().await;
        let result = match (result, cleanup) {
            (Ok(Ok(output)), Ok(())) => Ok(output),
            (Ok(Err(_)), Ok(())) => Err(TaskError::Cancelled),
            _ => Err(TaskError::Panicked),
        };
        // A detached handle has no receiver; the output is simply dropped here.
        let _ = sender.send(result);
    };
    #[cfg(not(target_arch = "wasm32"))]
    drop(tokio::spawn(task));
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(task);
    TaskHandle {
        cancellation,
        result,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::async_runtime::current_context;
    use futures_util::{future::pending, poll};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancel_before_first_poll_drops_once_without_running_business() {
        runtime().block_on(async {
            let dropped = Arc::new(AtomicUsize::new(0));
            let resource = CountDrop(dropped.clone());
            let handle = spawn(async move {
                let _resource = resource;
                panic!("cancelled business must not run");
            });
            handle.cancel();
            handle.cancel();
            assert_eq!(handle.cancel_and_wait().await, Err(TaskError::Cancelled));
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn wait_accepts_send_but_not_sync_state_and_output() {
        runtime().block_on(async {
            let state = std::cell::Cell::new(7);
            let handle = spawn(async move {
                tokio::task::yield_now().await;
                state
            });
            assert_eq!(handle.wait().await.unwrap().get(), 7);
        });
    }

    #[test]
    fn dropping_handle_detaches() {
        runtime().block_on(async {
            let (done, receiver) = oneshot::channel();
            drop(spawn(async move {
                done.send(42).unwrap();
            }));
            assert_eq!(receiver.await.unwrap(), 42);
        });
    }

    #[test]
    fn cancellation_waits_for_registered_cleanup() {
        runtime().block_on(async {
            let (started, ready) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let handle = spawn(async move {
                current_context()
                    .unwrap()
                    .defer_cleanup(async {
                        gate.await.unwrap();
                    })
                    .unwrap();
                started.send(()).unwrap();
                pending::<()>().await;
            });
            ready.await.unwrap();
            let mut closed = Box::pin(handle.cancel_and_wait());
            assert!(poll!(&mut closed).is_pending());
            tokio::task::yield_now().await;
            assert!(poll!(&mut closed).is_pending());
            release.send(()).unwrap();
            assert_eq!(closed.await, Err(TaskError::Cancelled));
        });
    }

    #[test]
    fn exposed_token_controls_the_same_task() {
        runtime().block_on(async {
            let handle = spawn(pending::<()>());
            let token = handle.cancellation_token();
            assert!(!token.is_cancelled());
            token.cancel();
            assert_eq!(handle.wait().await, Err(TaskError::Cancelled));
            assert!(token.is_cancelled());
        });
    }

    #[test]
    fn late_cancel_preserves_committed_output() {
        runtime().block_on(async {
            let (ready, started) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let handle = spawn(async move {
                current_context()
                    .unwrap()
                    .defer_cleanup(async {
                        gate.await.unwrap();
                    })
                    .unwrap();
                ready.send(()).unwrap();
                9
            });
            started.await.unwrap();
            handle.cancel();
            release.send(()).unwrap();
            assert_eq!(handle.cancel_and_wait().await, Ok(9));
        });
    }

    #[test]
    fn panic_waits_for_cleanup_and_reports_panicked() {
        runtime().block_on(async {
            let (started, ready) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            let handle = spawn(async move {
                current_context()
                    .unwrap()
                    .defer_cleanup(async {
                        gate.await.unwrap();
                    })
                    .unwrap();
                started.send(()).unwrap();
                panic!("business panic");
            });
            ready.await.unwrap();
            let mut wait = Box::pin(handle.wait());
            assert!(poll!(&mut wait).is_pending());
            release.send(()).unwrap();
            assert_eq!(wait.await, Err(TaskError::Panicked));
            assert_eq!(
                spawn(async {
                    current_context()
                        .unwrap()
                        .defer_cleanup(async { panic!("cleanup panic") })
                        .unwrap();
                })
                .wait()
                .await,
                Err(TaskError::Panicked)
            );
        });
    }

    #[test]
    fn stopped_executor_is_not_reported_as_successful_cancellation() {
        let executor = runtime();
        let handle = executor.block_on(async { spawn(pending::<()>()) });
        drop(executor);
        assert_eq!(
            runtime().block_on(handle.wait()),
            Err(TaskError::ExecutorStopped)
        );
    }

    #[test]
    fn executor_shutdown_before_first_poll_still_drops_in_call_scope() {
        struct ScopedDrop(Arc<AtomicUsize>);
        impl Drop for ScopedDrop {
            fn drop(&mut self) {
                assert!(current_context().is_some());
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let executor = runtime();
        let resource = ScopedDrop(dropped.clone());
        let handle = executor.block_on(async {
            spawn(async move {
                let _resource = resource;
                pending::<()>().await;
            })
        });
        drop(executor);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime().block_on(handle.wait()),
            Err(TaskError::ExecutorStopped)
        );
    }

    #[test]
    #[should_panic(expected = "there is no reactor running")]
    fn spawn_without_runtime_panics() {
        drop(spawn(async {}));
    }
}
