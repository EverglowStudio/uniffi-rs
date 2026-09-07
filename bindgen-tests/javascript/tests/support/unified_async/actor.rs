//! Business ledger facade over public Rust actor APIs; no actor protocol is exported.
use crate::{gate, open_gate, update};
use std::future::Future;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};
use uniffi::async_runtime::{
    actor::*, current_context, defer_local_cleanup, sleep, BoxFuture, LocalBoxFuture, LocalExecutor,
};

#[derive(Clone, Debug, uniffi::Error)]
pub enum ActorFixtureError {
    Rejected,
    Runtime { kind: String },
}
impl std::fmt::Display for ActorFixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected => f.write_str("ledger write rejected"),
            Self::Runtime { kind } => write!(f, "ledger lifecycle: {kind}"),
        }
    }
}
impl std::error::Error for ActorFixtureError {}
impl From<ActorError> for ActorFixtureError {
    fn from(error: ActorError) -> Self {
        Self::Runtime {
            kind: format!("{error:?}"),
        }
    }
}

#[derive(Clone, Default)]
struct Affinity {
    #[cfg(not(target_arch = "wasm32"))]
    thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
}
impl Affinity {
    fn set(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            *self.thread.lock().unwrap() = Some(std::thread::current().id());
        }
    }
    fn is_here(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.thread
                .lock()
                .unwrap()
                .is_none_or(|id| id == std::thread::current().id())
        }
        #[cfg(target_arch = "wasm32")]
        {
            true
        }
    }
    fn check(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(thread) = *self.thread.lock().unwrap() {
            assert_eq!(
                thread,
                std::thread::current().id(),
                "ledger resource escaped owner"
            );
        }
    }
}
trait Storage: 'static {
    fn get(&self) -> u32;
    fn set(&self, value: u32);
    fn cleanup(&self, id: u32, affinity: Affinity);
}
impl Storage for Cell<u32> {
    fn get(&self) -> u32 {
        Cell::get(self)
    }
    fn set(&self, value: u32) {
        Cell::set(self, value);
    }
    fn cleanup(&self, id: u32, affinity: Affinity) {
        current_context()
            .expect("actor body drop scope")
            .defer_cleanup(async move {
                affinity.check();
                update(id, |s| s.cleaning += 1);
                gate(id, 8, false).await;
                affinity.check();
                update(id, |s| s.cleaned += 1);
            })
            .unwrap();
    }
}
impl Storage for Rc<Cell<u32>> {
    fn get(&self) -> u32 {
        self.as_ref().get()
    }
    fn set(&self, value: u32) {
        self.as_ref().set(value);
    }
    fn cleanup(&self, id: u32, affinity: Affinity) {
        let local = self.clone();
        defer_local_cleanup(async move {
            affinity.check();
            update(id, |s| s.cleaning += 1);
            gate(id, 8, false).await;
            affinity.check();
            local.set(local.get() + 1);
            update(id, |s| s.cleaned += 1);
        })
        .unwrap();
    }
}
struct Ledger<S: Storage> {
    id: u32,
    value: S,
    affinity: Affinity,
}
impl<S: Storage> Actor for Ledger<S> {}
impl<S: Storage> Ledger<S> {
    fn new(id: u32, value: S, affinity: Affinity, local: bool) -> Self {
        if local {
            affinity.set();
        }
        update(id, |s| s.started += 1);
        open_gate(id, 16);
        Self {
            id,
            value,
            affinity,
        }
    }
    async fn apply(
        &mut self,
        command: Command,
        ctx: ActorContext,
    ) -> Result<u32, ActorFixtureError> {
        self.affinity.check();
        match command {
            Command::Read => {}
            Command::Write {
                payload,
                amount,
                mode,
            } => {
                let id = payload.id;
                update(id, |s| s.started += 1);
                // Both Cell and Rc survive real suspension before state access.
                sleep(Duration::ZERO).await;
                self.affinity.check();
                match mode {
                    1 => return Err(ActorFixtureError::Rejected),
                    2 => {
                        let cancelled = ctx.cancelled();
                        let ready = gate(id, 1, false);
                        futures_util::pin_mut!(cancelled, ready);
                        if matches!(
                            futures_util::future::select(cancelled, ready).await,
                            futures_util::future::Either::Left(_)
                        ) {
                            return Err(ActorError::Cancelled.into());
                        }
                        self.value.set(self.value.get() + amount);
                    }
                    3 => {
                        let saved = self.value.get();
                        self.value.set(9999);
                        ctx.cancelled().await;
                        update(id, |s| s.polls += 1);
                        gate(id, 1, false).await;
                        self.affinity.check();
                        self.value.set(saved + amount);
                        update(id, |s| s.nested += 1);
                    }
                    _ => self.value.set(self.value.get() + amount),
                }
                drop(payload);
            }
            Command::Schedule {
                id,
                amount,
                address,
            } => {
                ctx.spawn(move |task| async move {
                    update(id, |s| s.started += 1);
                    let cancelled = task.cancelled();
                    let ready = gate(id, 1, false);
                    futures_util::pin_mut!(cancelled, ready);
                    if matches!(
                        futures_util::future::select(cancelled, ready).await,
                        futures_util::future::Either::Left(_)
                    ) {
                        update(id, |s| s.cleaned += 1);
                        return Ok(());
                    }
                    address.complete(&task, id, amount).await?;
                    update(id, |s| s.produced += 1);
                    Ok(())
                })
                .map_err(ActorFixtureError::from)?;
            }
            Command::Complete { id, amount } => {
                self.value.set(self.value.get() + amount);
                update(id, |s| s.nested += 1);
            }
            Command::Fail => ctx.fail(ActorError::StartFailed),
        }
        self.affinity.check();
        Ok(self.value.get())
    }
}
impl<S: Storage> Drop for Ledger<S> {
    fn drop(&mut self) {
        self.affinity.check();
        update(self.id, |s| {
            s.dropped += 1;
            s.produced = self.value.get();
        });
        self.value.cleanup(self.id, self.affinity.clone());
    }
}
struct Payload {
    id: u32,
    affinity: Affinity,
    local: bool,
    cleanup: bool,
}
impl Drop for Payload {
    fn drop(&mut self) {
        let on_owner = self.affinity.is_here();
        let id = self.id;
        update(id, |s| {
            s.dropped += 1;
            s.owner_dropped += u32::from(on_owner);
        });
        if self.cleanup {
            let affinity = self.affinity.clone();
            let cleanup = async move {
                if on_owner {
                    affinity.check();
                }
                update(id, |s| s.cleaning += 1);
                gate(id, 2, false).await;
                if on_owner {
                    affinity.check();
                }
                update(id, |s| s.cleaned += 1);
            };
            if self.local && on_owner {
                defer_local_cleanup(cleanup).unwrap();
            } else {
                current_context()
                    .expect("actor payload drop scope")
                    .defer_cleanup(cleanup)
                    .unwrap();
            }
        }
    }
}
enum Command {
    Read,
    Write {
        payload: Payload,
        amount: u32,
        mode: u32,
    },
    Schedule {
        id: u32,
        amount: u32,
        address: Address,
    },
    Complete {
        id: u32,
        amount: u32,
    },
    Fail,
}
impl Message for Command {
    type Reply = Result<u32, ActorFixtureError>;
}
impl Handler<Command> for Ledger<Cell<u32>> {
    type Future<'a> = BoxFuture<'a, Result<u32, ActorFixtureError>>;
    fn handle<'a>(&'a mut self, command: Command, ctx: ActorContext) -> Self::Future<'a> {
        Box::pin(self.apply(command, ctx))
    }
}
impl Handler<Command> for Ledger<Rc<Cell<u32>>> {
    type Future<'a> = LocalBoxFuture<'a, Result<u32, ActorFixtureError>>;
    fn handle<'a>(&'a mut self, command: Command, ctx: ActorContext) -> Self::Future<'a> {
        Box::pin(self.apply(command, ctx))
    }
}
#[derive(Clone)]
enum Address {
    Send(ActorRef<Ledger<Cell<u32>>>),
    Local(LocalActorRef<Ledger<Rc<Cell<u32>>>>),
}
impl Address {
    fn ask(&self, command: Command) -> Ask<Result<u32, ActorFixtureError>> {
        match self {
            Self::Send(a) => a.ask(command),
            Self::Local(a) => a.ask(command),
        }
    }
    async fn complete(
        &self,
        task: &ManagedContext,
        id: u32,
        amount: u32,
    ) -> Result<(), ActorError> {
        match self {
            Self::Send(a) => task.send(a, Command::Complete { id, amount }).await,
            Self::Local(a) => task.send(a, Command::Complete { id, amount }).await,
        }
    }
}

#[derive(uniffi::Object)]
pub struct ActorAccount {
    owner: ActorOwner,
    address: Address,
    affinity: Affinity,
    local: bool,
}
#[uniffi::export]
impl ActorAccount {
    #[uniffi::constructor]
    pub async fn new(local: bool, id: u32, value: u32) -> Result<Arc<Self>, ActorFixtureError> {
        let affinity = Affinity::default();
        let (owner, address) = if local {
            let executor = LocalExecutor::new(8).map_err(|error| ActorFixtureError::Runtime {
                kind: format!("{error:?}"),
            })?;
            let thread = affinity.clone();
            let (owner, address) = start_local(
                &executor,
                move || Ledger::new(id, Rc::new(Cell::new(value)), thread, true),
                2,
            )
            .await?;
            (owner, Address::Local(address))
        } else {
            let (owner, address) = start(
                Ledger::new(id, Cell::new(value), affinity.clone(), false),
                2,
            )?;
            (owner, Address::Send(address))
        };
        gate(id, 16, false).await;
        Ok(Arc::new(Self {
            owner,
            address,
            affinity,
            local,
        }))
    }
    pub async fn value(&self) -> Result<u32, ActorFixtureError> {
        self.address.ask(Command::Read).await?
    }
    /// Mode 0 writes, 1 rejects, 2 waits for business gate, 3 recovers on cancel.
    pub async fn write(
        &self,
        id: u32,
        amount: u32,
        mode: u32,
        cleanup: bool,
    ) -> Result<u32, ActorFixtureError> {
        let mut ask = Box::pin(self.address.ask(Command::Write {
            payload: Payload {
                id,
                affinity: self.affinity.clone(),
                local: self.local,
                cleanup,
            },
            amount,
            mode,
        }));
        let mut first = true;
        std::future::poll_fn(|cx| {
            let result = ask.as_mut().poll(cx);
            if std::mem::take(&mut first) {
                update(id, |s| s.produced += 1);
            }
            result
        })
        .await?
    }
    pub async fn schedule(&self, id: u32, amount: u32) -> Result<u32, ActorFixtureError> {
        self.address
            .ask(Command::Schedule {
                id,
                amount,
                address: self.address.clone(),
            })
            .await?
    }
    pub async fn fail(&self) -> Result<u32, ActorFixtureError> {
        self.address.ask(Command::Fail).await?
    }
    pub async fn close(&self) -> Result<(), ActorFixtureError> {
        self.owner.close().await.map_err(Into::into)
    }
    pub async fn drain(&self) -> Result<(), ActorFixtureError> {
        self.owner.drain_and_close().await.map_err(Into::into)
    }
}
