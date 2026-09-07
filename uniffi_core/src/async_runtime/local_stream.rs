//! Demand-driven local Stream proxy. The business stream stays on its owner.

use super::{
    current_context, local::store_current_local, spawn_local, LocalError, LocalObject,
    SendOutsideWasm, TaskError, TaskHandle,
};
use crate::UniFfiStream;
use futures_channel::oneshot;
use futures_core::Stream;
use futures_util::future::poll_fn;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

type LocalStream<T, E> = Pin<Box<dyn Stream<Item = Result<T, E>>>>;
type StoredStream<T, E> = Rc<RefCell<Option<LocalStream<T, E>>>>;
type Reply<T, E> = oneshot::Sender<Option<Result<T, E>>>;

/// Turn an already constructed, potentially !Send stream into a transferable
/// subscription. Must be called inside a LocalExecutor owner.
///
/// The owner task waits for demand before polling the business stream. It holds
/// one subscription CallContext across all next requests, so poll-registered
/// cleanup runs only after the stream's destructor. At most one request and one
/// undelivered item exist. The FFI driver reports owner cancellation/failure as
/// a framework terminal, separately from E. A direct Rust Stream consumer sees
/// the end of the subscription after the owner task has finished its cleanup.
pub fn local_stream<S, T, E>(stream: S) -> Result<UniFfiStream<T, E>, LocalError>
where
    S: Stream<Item = Result<T, E>> + 'static,
    T: SendOutsideWasm + 'static,
    E: SendOutsideWasm + 'static,
{
    let stream: LocalStream<T, E> = Box::pin(stream);
    let stored = Rc::new(RefCell::new(Some(stream)));
    let object = store_current_local(stored.clone())?;
    let (requests, mut receiver) = mpsc::channel::<Reply<T, E>>(1);
    // This guard is captured before spawn so cancellation before first poll also
    // removes/drops the stream in the owner task's Run scope. LocalObject only
    // stores the local value; no Rc or raw Stream crosses the owner boundary.
    let owned = OwnerStream(stored);
    let task = spawn_local(async move {
        let owned = owned;
        while let Some(reply) = receiver.recv().await {
            let item = poll_fn(|cx| {
                owned
                    .0
                    .borrow_mut()
                    .as_mut()
                    .expect("live local stream")
                    .as_mut()
                    .poll_next(cx)
            })
            .await;
            let terminal = matches!(item, None | Some(Err(_)));
            drop(reply.send(item));
            if terminal {
                break;
            }
        }
        drop(owned);
    })?;
    Ok(Box::pin(LocalStreamProxy {
        object: Some(object),
        task: Some(task),
        requests,
        pending: None,
        owner_stopped: false,
        done: false,
    }))
}

struct OwnerStream<T, E>(StoredStream<T, E>);
impl<T, E> Drop for OwnerStream<T, E> {
    fn drop(&mut self) {
        let stream = self.0.borrow_mut().take();
        drop(stream);
    }
}

struct LocalStreamProxy<T: 'static, E: 'static> {
    object: Option<LocalObject<StoredStream<T, E>>>,
    task: Option<TaskHandle<()>>,
    requests: mpsc::Sender<Reply<T, E>>,
    pending: Option<oneshot::Receiver<Option<Result<T, E>>>>,
    owner_stopped: bool,
    done: bool,
}
impl<T, E> Unpin for LocalStreamProxy<T, E> {}
impl<T: SendOutsideWasm + 'static, E: SendOutsideWasm + 'static> Stream for LocalStreamProxy<T, E> {
    type Item = Result<T, E>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.owner_stopped {
            return self.poll_owner_stopped(cx);
        }
        if self.pending.is_none() {
            let (reply, response) = oneshot::channel();
            if self.requests.try_send(reply).is_err() {
                self.owner_stopped = true;
                return self.poll_owner_stopped(cx);
            }
            self.pending = Some(response);
        }
        match Pin::new(self.pending.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                let Ok(item) = result else {
                    self.owner_stopped = true;
                    return self.poll_owner_stopped(cx);
                };
                if matches!(item, None | Some(Err(_))) {
                    self.done = true;
                    self.release();
                }
                Poll::Ready(item)
            }
        }
    }
}
impl<T, E> LocalStreamProxy<T, E> {
    fn poll_owner_stopped(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, E>>> {
        let task = self.task.as_mut().expect("live subscription owns a task");
        task.cancel();
        // A dropped reply only marks the end of business execution. The task
        // result is sent after owner-local destruction and registered cleanup.
        let result = match Pin::new(&mut task.result).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result.unwrap_or(Err(TaskError::ExecutorStopped)),
        };
        drop(self.task.take());
        self.done = true;
        crate::ffi::ruststream::local_stream_stopped(
            result.err().unwrap_or(TaskError::ExecutorStopped),
        );
        self.release();
        Poll::Ready(None)
    }

    fn release(&mut self) {
        if let Some(task) = self.task.take() {
            task.cancel();
            if let Some(context) = current_context() {
                context
                    .defer_cleanup(async move {
                        match task.wait().await {
                            Ok(()) | Err(TaskError::Cancelled) => (),
                            Err(error) => panic!("local stream cleanup failed: {error}"),
                        }
                    })
                    .expect("current call accepts local stream cleanup");
            }
        }
        drop(self.object.take());
    }
}
impl<T, E> Drop for LocalStreamProxy<T, E> {
    fn drop(&mut self) {
        self.release();
        drop(self.pending.take());
    }
}
