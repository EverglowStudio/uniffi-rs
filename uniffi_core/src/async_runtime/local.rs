//! Fixed-owner local tasks and typed, transferable object proxies.

use super::{
    current_context, CallContext, CancellationToken, SendOutsideWasm, TaskError, TaskHandle,
};
use futures_channel::oneshot;
use futures_util::{
    future::{poll_fn, Shared},
    stream::FuturesUnordered,
    FutureExt, Stream,
};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    future::Future,
    marker::PhantomData,
    panic::AssertUnwindSafe,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::{mpsc, Notify};

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
#[cfg(not(target_arch = "wasm32"))]
type Command = Box<dyn FnOnce(Rc<Owner>) + Send>;
#[cfg(target_arch = "wasm32")]
type Command = Box<dyn FnOnce(Rc<Owner>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalError {
    Closed,
    Cancelled,
    Panicked,
    ExecutorStopped,
    NoOwner,
    InvalidCapacity,
    StartFailed,
}
impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "local owner or object closed",
            Self::Cancelled => "local call cancelled",
            Self::Panicked => "local task or cleanup panicked",
            Self::ExecutorStopped => "local executor stopped",
            Self::NoOwner => "no current local owner",
            Self::InvalidCapacity => "local capacity must be nonzero",
            Self::StartFailed => "failed to start local owner",
        })
    }
}
impl std::error::Error for LocalError {}
impl From<TaskError> for LocalError {
    fn from(e: TaskError) -> Self {
        match e {
            TaskError::Cancelled => Self::Cancelled,
            TaskError::Panicked => Self::Panicked,
            TaskError::ExecutorStopped => Self::ExecutorStopped,
        }
    }
}

type Signal = Shared<oneshot::Receiver<Result<(), LocalError>>>;
struct Completion {
    sender: Mutex<Option<oneshot::Sender<Result<(), LocalError>>>>,
    signal: Signal,
}
impl Completion {
    fn new() -> Arc<Self> {
        let (sender, receiver) = oneshot::channel();
        Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            signal: receiver.shared(),
        })
    }
    fn finish(&self, result: Result<(), LocalError>) {
        if let Some(sender) = self.sender.lock().unwrap().take() {
            let _ = sender.send(result);
        }
    }
    // Once close has started, dropping its waiter transfers confirmation to the
    // surrounding S01 cleanup (or the current owner request's release root).
    async fn wait_owned(self: &Arc<Self>) -> Result<(), LocalError> {
        let mut guard = ReleaseOnDrop(Some(self.clone()));
        let result = self.wait().await;
        guard.0 = None;
        result
    }
    async fn wait(&self) -> Result<(), LocalError> {
        self.signal
            .clone()
            .await
            .unwrap_or(Err(LocalError::ExecutorStopped))
    }
}
struct Control {
    // Recover the current executor for owner-only storage, without a strong cycle.
    executor: Mutex<std::sync::Weak<ExecutorHandle>>,
    stop: CancellationToken,
    changed: Notify,
    done: Arc<Completion>,
}
struct ExecutorHandle {
    sender: mpsc::Sender<Packet>,
    control: Arc<Control>,
}
impl Drop for ExecutorHandle {
    fn drop(&mut self) {
        self.control.stop.cancel();
        self.control.changed.notify_one();
    }
}
/// A reusable owner. Native creates one current-thread Tokio runtime and LocalSet;
/// WASM uses the current JavaScript instance's event loop.
#[derive(Clone)]
pub struct LocalExecutor {
    handle: Arc<ExecutorHandle>,
}
struct Packet {
    command: Option<Command>,
    done: Arc<Completion>,
}
impl Packet {
    fn execute(mut self, owner: Rc<Owner>) {
        self.command.take().unwrap()(owner);
    }
}
impl Drop for Packet {
    fn drop(&mut self) {
        if self.command.is_some() {
            drop(self.command.take());
            self.done.finish(Ok(()));
        }
    }
}
struct RequestGuard {
    token: CancellationToken,
    done: Arc<Completion>,
    armed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
            if let Some(parent) = current_context() {
                let done = self.done.clone();
                parent
                    .defer_cleanup(async move {
                        done.wait().await.expect("local request cleanup failed");
                    })
                    .expect("current context must accept local request cleanup");
            }
        }
    }
}
struct ObjectControl {
    stop: CancellationToken,
    done: Arc<Completion>,
    owner: Arc<Control>,
}
impl ObjectControl {
    fn cancel(&self) {
        self.stop.cancel();
        self.owner.changed.notify_one();
    }
}
struct Proxy {
    executor: LocalExecutor,
    id: u64,
    control: Arc<ObjectControl>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.control.cancel();
        defer_release(self.control.done.clone());
    }
}
struct ReleaseOnDrop(Option<Arc<Completion>>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            defer_release(done);
        }
    }
}
fn defer_release(done: Arc<Completion>) {
    if RELEASES.with(|slot| {
        if let Some(releases) = slot.borrow().as_ref() {
            releases.pending.borrow_mut().push(done.clone());
            true
        } else {
            false
        }
    }) {
        return;
    }
    if let Some(context) = current_context() {
        context
            .defer_cleanup(async move {
                done.wait().await.expect("local release cleanup failed");
            })
            .expect("current context must accept local release cleanup");
    }
}
/// A typed ID and owner control, never an exposed native `Rc<T>` or `&T`.
/// All construction, access and destruction of T happens on the owner.
///
/// ```compile_fail
/// use std::rc::Rc;
/// use uniffi_core::async_runtime::LocalObject;
/// async fn escape(proxy: LocalObject<Rc<u32>>) {
///     let _: Rc<u32> = proxy.call(|v| Box::pin(async move { v.clone() })).await.unwrap();
/// }
/// ```
/// ```compile_fail
/// use uniffi_core::async_runtime::LocalObject;
/// async fn borrow(proxy: LocalObject<u32>) {
///     let _: &u32 = proxy.call(|v| Box::pin(async move { v })).await.unwrap();
/// }
/// ```
pub struct LocalObject<T: 'static> {
    proxy: Arc<Proxy>,
    marker: PhantomData<fn() -> T>,
}
impl<T> Clone for LocalObject<T> {
    fn clone(&self) -> Self {
        Self {
            proxy: self.proxy.clone(),
            marker: PhantomData,
        }
    }
}
impl<T> fmt::Debug for LocalObject<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalObject")
            .field("id", &self.proxy.id)
            .finish_non_exhaustive()
    }
}
struct Entry {
    value: Rc<dyn Any>,
    control: Arc<ObjectControl>,
}
struct Active {
    token: CancellationToken,
    object: Option<u64>,
}
struct Owner {
    control: Arc<Control>,
    next: Cell<u64>,
    objects: RefCell<HashMap<u64, Entry>>,
    active: RefCell<HashMap<u64, Active>>,
    releases: RefCell<HashMap<u64, Releases>>,
    pending: RefCell<Vec<LocalBoxFuture<'static, ()>>>,
}
thread_local! { static CURRENT: RefCell<Option<Rc<Owner>>> = const { RefCell::new(None) }; }
// Directly dependent requests on the same owner share a release root. A child
// confirms its own business/cleanup and leaves active before its parent resumes;
// the outermost request confirms proxy releases after the whole dependency chain
// leaves active. Independent spawn tasks always create their own root.
struct ReleaseRoot {
    id: u64,
    pending: RefCell<Vec<Arc<Completion>>>,
}
type Releases = Rc<ReleaseRoot>;
thread_local! { static RELEASES: RefCell<Option<Releases>> = const { RefCell::new(None) }; }
struct ReleaseScope(Option<Releases>);
impl ReleaseScope {
    fn enter(releases: &Option<Releases>) -> Self {
        Self(RELEASES.with(|s| s.replace(releases.clone())))
    }
}
impl Drop for ReleaseScope {
    fn drop(&mut self) {
        RELEASES.with(|s| {
            s.replace(self.0.take());
        });
    }
}
struct WithReleases<F: Future> {
    releases: Option<Releases>,
    future: Option<Pin<Box<F>>>,
}
impl<F: Future> Future for WithReleases<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _scope = ReleaseScope::enter(&this.releases);
        this.future.as_mut().unwrap().as_mut().poll(cx)
    }
}
impl<F: Future> Drop for WithReleases<F> {
    fn drop(&mut self) {
        let _scope = ReleaseScope::enter(&self.releases);
        drop(self.future.take());
    }
}
async fn wait_releases(releases: &Releases) -> Result<(), LocalError> {
    let mut result = Ok(());
    loop {
        let pending = std::mem::take(&mut *releases.pending.borrow_mut());
        if pending.is_empty() {
            return result;
        }
        for release in pending {
            if let Err(error) = release.wait().await {
                result = Err(error);
            }
        }
    }
}
struct Scope(Option<Rc<Owner>>);
impl Scope {
    fn enter(owner: &Rc<Owner>) -> Self {
        Self(CURRENT.with(|s| s.replace(Some(owner.clone()))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|s| {
            s.replace(self.0.take());
        });
    }
}
struct InOwner<F: Future> {
    owner: Rc<Owner>,
    future: Option<Pin<Box<F>>>,
}
impl<F: Future> Future for InOwner<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _scope = Scope::enter(&this.owner);
        this.future.as_mut().unwrap().as_mut().poll(cx)
    }
}
impl<F: Future> Drop for InOwner<F> {
    fn drop(&mut self) {
        let _scope = Scope::enter(&self.owner);
        drop(self.future.take());
    }
}
impl Owner {
    fn id(&self) -> u64 {
        let id = self.next.get();
        self.next
            .set(id.checked_add(1).expect("local ID exhausted"));
        id
    }
    fn push(&self, task: LocalBoxFuture<'static, ()>) {
        self.pending.borrow_mut().push(task);
        self.control.changed.notify_one();
    }
    fn start<F: Future + 'static>(
        self: &Rc<Self>,
        future: F,
        token: CancellationToken,
        object: Option<u64>,
        done: Arc<Completion>,
        parent_root: Option<u64>,
    ) -> TaskHandle<F::Output>
    where
        F::Output: 'static,
    {
        let context = CallContext::new(token.clone());
        let run = context.run(future);
        let (sender, result) = oneshot::channel();
        let id = self.id();
        let releases = match parent_root {
            Some(root) => self
                .releases
                .borrow()
                .get(&root)
                .expect("dependent request retains its release root until child completion")
                .clone(),
            None => {
                let releases = Rc::new(ReleaseRoot {
                    id,
                    pending: RefCell::new(Vec::new()),
                });
                self.releases.borrow_mut().insert(id, releases.clone());
                releases
            }
        };
        self.active.borrow_mut().insert(
            id,
            Active {
                token: token.clone(),
                object,
            },
        );
        let owner = self.clone();
        let task_releases = releases.clone();
        let task = async move {
            let result = AssertUnwindSafe(run).catch_unwind().await;
            let cleanup = context.wait_closed().await;
            let mut result = match (result, cleanup) {
                (Ok(Ok(value)), Ok(())) => Ok(value),
                (Ok(Err(_)), Ok(())) => Err(TaskError::Cancelled),
                _ => Err(TaskError::Panicked),
            };
            let completion = if matches!(result, Err(TaskError::Panicked)) {
                Err(LocalError::Panicked)
            } else {
                Ok(())
            };
            // If the receiver disappeared, returned resources are dropped on owner
            // before acknowledging release (including their proxy finalizers).
            owner.active.borrow_mut().remove(&id);
            owner.control.changed.notify_one();
            let business_released = if parent_root.is_none() {
                wait_releases(&task_releases).await
            } else {
                Ok(())
            };
            if business_released.is_err() {
                result = Err(TaskError::Panicked);
            }
            let delivery = CallContext::new(CancellationToken::new());
            let delivered = AssertUnwindSafe(delivery.run(async move {
                drop(sender.send(result));
            }))
            .catch_unwind()
            .await;
            let released = delivery.wait_closed().await;
            let outputs_released = if parent_root.is_none() {
                wait_releases(&task_releases).await
            } else {
                Ok(())
            };
            if parent_root.is_none() {
                owner.releases.borrow_mut().remove(&id);
            }
            done.finish(
                if delivered.is_ok()
                    && released.is_ok()
                    && business_released.is_ok()
                    && outputs_released.is_ok()
                {
                    completion
                } else {
                    Err(LocalError::Panicked)
                },
            );
            owner.control.changed.notify_one();
        };
        self.push(Box::pin(WithReleases {
            releases: Some(releases),
            future: Some(Box::pin(task)),
        }));
        TaskHandle {
            cancellation: token,
            result,
        }
    }
    fn close_objects(self: &Rc<Self>) {
        let ids: Vec<_> = self
            .objects
            .borrow()
            .iter()
            .filter_map(|(&id, entry)| {
                (entry.control.stop.is_cancelled()
                    && !self
                        .active
                        .borrow()
                        .values()
                        .any(|task| task.object == Some(id)))
                .then_some(id)
            })
            .collect();
        for id in ids {
            let entry = self.objects.borrow_mut().remove(&id).unwrap();
            let done = entry.control.done.clone();
            let handle = self.start(
                async move {
                    drop(entry.value);
                },
                CancellationToken::new(),
                None,
                done,
                None,
            );
            drop(handle);
        }
    }
}

impl LocalExecutor {
    pub fn new(capacity: usize) -> Result<Self, LocalError> {
        if capacity == 0 || capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(LocalError::InvalidCapacity);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        let control = Arc::new(Control {
            executor: Mutex::new(std::sync::Weak::new()),
            stop: CancellationToken::new(),
            changed: Notify::new(),
            done: Completion::new(),
        });
        let worker_control = control.clone();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| LocalError::StartFailed)?;
            std::thread::Builder::new()
                .name("uniffi-local".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        let local = tokio::task::LocalSet::new();
                        local.block_on(&runtime, drive(worker_control.clone(), receiver));
                        drop(local);
                        drop(runtime);
                    }));
                    worker_control
                        .done
                        .finish(result.map_err(|_| LocalError::ExecutorStopped));
                })
                .map_err(|_| LocalError::StartFailed)?;
        }
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            let result = AssertUnwindSafe(drive(worker_control.clone(), receiver))
                .catch_unwind()
                .await;
            worker_control
                .done
                .finish(result.map_err(|_| LocalError::ExecutorStopped));
        });
        let handle = Arc::new(ExecutorHandle { sender, control });
        *handle.control.executor.lock().unwrap() = Arc::downgrade(&handle);
        Ok(Self { handle })
    }
    /// Request shutdown independently of queue capacity.
    pub fn cancel(&self) {
        self.handle.control.stop.cancel();
        self.handle.control.changed.notify_one();
    }
    /// Cancel owned tasks, drop objects, finish cleanup and exit the owner.
    /// Concurrent/repeated callers share completion. Do not await this from an
    /// owned task: that task is itself part of the shutdown being awaited.
    /// Dropping a started waiter transfers confirmation to the current call's cleanup.
    pub async fn close(&self) -> Result<(), LocalError> {
        self.cancel();
        self.handle.control.done.wait_owned().await
    }
    async fn submit(&self, packet: Packet, token: &CancellationToken) -> Result<(), LocalError> {
        let control = &self.handle.control;
        let mut reserve = Box::pin(self.handle.sender.reserve());
        let mut stopped = Box::pin(control.stop.cancelled());
        let mut cancelled = Box::pin(token.cancelled());
        let permit = poll_fn(|cx| {
            if stopped.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(LocalError::Closed));
            }
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(LocalError::Cancelled));
            }
            reserve
                .as_mut()
                .poll(cx)
                .map(|r| r.map_err(|_| LocalError::Closed))
        })
        .await?;
        permit.send(packet);
        Ok(())
    }
    /// Asynchronously wait for bounded queue capacity, then submit an independent
    /// task. The Send factory runs on owner; its future and captures may be !Send.
    ///
    /// ```compile_fail
    /// use std::rc::Rc;
    /// use uniffi_core::async_runtime::LocalExecutor;
    /// async fn cannot_transfer_existing_rc(owner: LocalExecutor) {
    ///     let local = Rc::new(1);
    ///     owner.spawn(move || async move { *local }).await.unwrap();
    /// }
    /// ```
    pub async fn spawn<F, Fut, R>(&self, factory: F) -> Result<TaskHandle<R>, LocalError>
    where
        F: FnOnce() -> Fut + SendOutsideWasm + 'static,
        Fut: Future<Output = R> + 'static,
        R: SendOutsideWasm + 'static,
    {
        self.dispatch(factory, None, None)
            .await
            .map(|(handle, _)| handle)
    }
    fn dependency_root(&self) -> Option<u64> {
        CURRENT.with(|current| {
            if current
                .borrow()
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(&owner.control, &self.handle.control))
            {
                RELEASES.with(|slot| slot.borrow().as_ref().map(|root| root.id))
            } else {
                None
            }
        })
    }
    async fn dispatch<F, Fut, R>(
        &self,
        factory: F,
        object: Option<Arc<Proxy>>,
        parent_root: Option<u64>,
    ) -> Result<(TaskHandle<R>, Arc<Completion>), LocalError>
    where
        F: FnOnce() -> Fut + SendOutsideWasm + 'static,
        Fut: Future<Output = R> + 'static,
        R: SendOutsideWasm + 'static,
    {
        let token = CancellationToken::new();
        let done = Completion::new();
        let mut guard = RequestGuard {
            token: token.clone(),
            done: done.clone(),
            armed: true,
        };
        let (sender, result) = oneshot::channel();
        let job_token = token.clone();
        let job_done = done.clone();
        let command = Box::new(move |owner: Rc<Owner>| {
            let object_id = object.as_ref().map(|p| p.id);
            let closed = owner.control.stop.is_cancelled()
                || object
                    .as_ref()
                    .is_some_and(|p| p.control.stop.is_cancelled());
            if closed {
                job_token.cancel();
            }
            // The factory is called inside Run, so cancellation before first poll
            // never constructs local state, and a factory panic is contained.
            let handle = owner.start(
                async move { factory().await },
                job_token,
                object_id,
                job_done,
                parent_root,
            );
            drop(sender.send(handle));
        });
        self.submit(
            Packet {
                command: Some(command),
                done: done.clone(),
            },
            &token,
        )
        .await?;
        let handle = result.await.map_err(|_| LocalError::Closed)?;
        guard.armed = false;
        Ok((handle, done))
    }
    /// Construct T on this owner and return a transferable proxy.
    ///
    /// ```
    /// # #[cfg(not(target_arch = "wasm32"))]
    /// # futures::executor::block_on(async {
    /// use std::cell::Cell;
    /// use uniffi_core::async_runtime::LocalExecutor;
    /// let owner = LocalExecutor::new(8).unwrap();
    /// let object = owner.create(|| async { Cell::new(0) }).await.unwrap();
    /// let value = object.call(|v| Box::pin(async move {
    ///     v.set(v.get() + 1);
    ///     v.get()
    /// })).await.unwrap();
    /// assert_eq!(value, 1);
    /// object.close().await.unwrap();
    /// owner.close().await.unwrap();
    /// # });
    /// ```
    pub async fn create<T, F, Fut>(&self, factory: F) -> Result<LocalObject<T>, LocalError>
    where
        T: 'static,
        F: FnOnce() -> Fut + SendOutsideWasm + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let executor = self.clone();
        let (handle, done) = self
            .dispatch(
                move || async move {
                    let value = factory().await;
                    let owner = CURRENT
                        .with(|s| s.borrow().clone())
                        .expect("owner factory scope");
                    let id = owner.id();
                    let control = Arc::new(ObjectControl {
                        stop: CancellationToken::new(),
                        done: Completion::new(),
                        owner: owner.control.clone(),
                    });
                    owner.objects.borrow_mut().insert(
                        id,
                        Entry {
                            value: Rc::new(value),
                            control: control.clone(),
                        },
                    );
                    LocalObject {
                        proxy: Arc::new(Proxy {
                            executor,
                            id,
                            control,
                        }),
                        marker: PhantomData,
                    }
                },
                None,
                self.dependency_root(),
            )
            .await?;
        wait_request(handle, done).await
    }
}
async fn wait_request<R>(handle: TaskHandle<R>, done: Arc<Completion>) -> Result<R, LocalError> {
    let mut guard = RequestGuard {
        token: handle.cancellation_token(),
        done,
        armed: true,
    };
    let result = handle.wait().await.map_err(LocalError::from);
    guard.armed = false;
    result
}
impl<T: 'static> LocalObject<T> {
    /// Call without holding any registry borrow across await. Separate methods
    /// can interleave and can reenter this object through another proxy.
    pub async fn call<R, F>(&self, method: F) -> Result<R, LocalError>
    where
        R: SendOutsideWasm + 'static,
        F: for<'a> FnOnce(&'a T) -> LocalBoxFuture<'a, R> + SendOutsideWasm + 'static,
    {
        if self.proxy.control.stop.is_cancelled()
            || self.proxy.executor.handle.control.stop.is_cancelled()
        {
            return Err(LocalError::Closed);
        }
        let id = self.proxy.id;
        let (handle, done) = self
            .proxy
            .executor
            .dispatch(
                move || async move {
                    let value = CURRENT.with(|s| {
                        s.borrow()
                            .as_ref()
                            .unwrap()
                            .objects
                            .borrow()
                            .get(&id)
                            .map(|entry| entry.value.clone())
                    });
                    let value = value.ok_or(LocalError::Closed)?;
                    let value = value.downcast::<T>().expect("typed local object ID");
                    Ok(method(&value).await)
                },
                Some(self.proxy.clone()),
                self.proxy.executor.dependency_root(),
            )
            .await?;
        wait_request(handle, done).await?
    }
    pub fn cancel(&self) {
        self.proxy.control.cancel();
    }
    /// Close the object and confirm its release. Dropping a started waiter
    /// transfers confirmation to the current call's cleanup.
    pub async fn close(&self) -> Result<(), LocalError> {
        self.cancel();
        self.proxy.control.done.wait_owned().await
    }
}

/// Spawn a !Send task only in a valid local owner scope. Handle drop detaches;
/// owner shutdown still cancels and waits for the task and its cleanup.
pub fn spawn_local<F>(future: F) -> Result<TaskHandle<F::Output>, LocalError>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let owner = CURRENT
        .with(|s| s.borrow().clone())
        .ok_or(LocalError::NoOwner)?;
    if owner.control.stop.is_cancelled() {
        return Err(LocalError::Closed);
    }
    Ok(owner.start(
        future,
        CancellationToken::new(),
        None,
        Completion::new(),
        None,
    ))
}
/// Register !Send cleanup in the current owner and call. The shared CallContext
/// contains only start/completion channels; the cleanup future never leaves owner.
pub fn defer_local_cleanup<F>(future: F) -> Result<(), LocalError>
where
    F: Future<Output = ()> + 'static,
{
    let owner = CURRENT
        .with(|s| s.borrow().clone())
        .ok_or(LocalError::NoOwner)?;
    let context = current_context().ok_or(LocalError::NoOwner)?;
    let (start, started) = oneshot::channel();
    let (done, finished) = oneshot::channel::<Result<(), LocalError>>();
    context
        .defer_cleanup(async move {
            let _ = start.send(());
            finished
                .await
                .expect("local cleanup driver stopped")
                .expect("local cleanup panicked");
        })
        .map_err(|_| LocalError::Closed)?;
    let releases = RELEASES.with(|slot| slot.borrow().clone());
    let cleanup_task = async move {
        if started.await.is_ok() {
            let cleanup_context = CallContext::new(CancellationToken::new());
            let result = AssertUnwindSafe(cleanup_context.run(future))
                .catch_unwind()
                .await;
            let cleanup = cleanup_context.wait_closed().await;
            let _ = done.send(if result.is_ok() && cleanup.is_ok() {
                Ok(())
            } else {
                Err(LocalError::Panicked)
            });
        }
    };
    owner.push(Box::pin(WithReleases {
        releases,
        future: Some(Box::pin(cleanup_task)),
    }));
    Ok(())
}
async fn drive(control: Arc<Control>, mut receiver: mpsc::Receiver<Packet>) {
    let owner = Rc::new(Owner {
        control: control.clone(),
        next: Cell::new(0),
        objects: RefCell::new(HashMap::new()),
        active: RefCell::new(HashMap::new()),
        releases: RefCell::new(HashMap::new()),
        pending: RefCell::new(Vec::new()),
    });
    let state = owner.clone();
    let driver = async move {
        let mut running = FuturesUnordered::new();
        let mut stopped = false;
        loop {
            if control.stop.is_cancelled() {
                stopped = true;
                receiver.close();
                while let Ok(packet) = receiver.try_recv() {
                    drop(packet);
                }
                for entry in state.objects.borrow().values() {
                    entry.control.stop.cancel();
                }
                for active in state.active.borrow().values() {
                    active.token.cancel();
                }
            }
            for active in state.active.borrow().values() {
                if active.object.is_some_and(|id| {
                    state
                        .objects
                        .borrow()
                        .get(&id)
                        .is_some_and(|entry| entry.control.stop.is_cancelled())
                }) {
                    active.token.cancel();
                }
            }
            state.close_objects();
            for task in state.pending.borrow_mut().drain(..) {
                running.push(task);
            }
            if stopped && running.is_empty() && state.objects.borrow().is_empty() {
                break;
            }
            let mut changed = Box::pin(control.changed.notified());
            poll_fn(|cx| {
                if changed.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                if let Poll::Ready(Some(())) = Pin::new(&mut running).poll_next(cx) {
                    return Poll::Ready(());
                }
                if !stopped {
                    match receiver.poll_recv(cx) {
                        Poll::Ready(Some(packet)) => {
                            packet.execute(state.clone());
                            return Poll::Ready(());
                        }
                        Poll::Ready(None) => {
                            control.stop.cancel();
                            return Poll::Ready(());
                        }
                        Poll::Pending => {}
                    }
                }
                Poll::Pending
            })
            .await;
        }
    };
    InOwner {
        owner,
        future: Some(Box::pin(driver)),
    }
    .await;
}

/// Store an already constructed value on the current owner. The value never
/// crosses a thread: only the same typed ID/control used by create is returned.
/// This narrow entry point is used by local_stream after local method execution.
pub(super) fn store_current_local<T: 'static>(value: T) -> Result<LocalObject<T>, LocalError> {
    let owner = CURRENT
        .with(|slot| slot.borrow().clone())
        .ok_or(LocalError::NoOwner)?;
    if owner.control.stop.is_cancelled() {
        return Err(LocalError::Closed);
    }
    let handle = owner
        .control
        .executor
        .lock()
        .unwrap()
        .upgrade()
        .ok_or(LocalError::Closed)?;
    let id = owner.id();
    let control = Arc::new(ObjectControl {
        stop: CancellationToken::new(),
        done: Completion::new(),
        owner: owner.control.clone(),
    });
    owner.objects.borrow_mut().insert(
        id,
        Entry {
            value: Rc::new(value),
            control: control.clone(),
        },
    );
    Ok(LocalObject {
        proxy: Arc::new(Proxy {
            executor: LocalExecutor { handle },
            id,
            control,
        }),
        marker: PhantomData,
    })
}
