//! Standalone Rust actor smoke fixture. Raw status exports keep this independent
//! of the business FFI harness (which is exercised by the integration owner).
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[cfg(target_arch = "wasm32")]
mod wasm {
    use futures::{channel::oneshot, poll};
    use std::{
        cell::Cell,
        rc::Rc,
        sync::atomic::{AtomicU32, Ordering},
        time::Duration,
    };
    use uniffi_core::async_runtime::{
        actor::*, current_context, BoxFuture, CallContext, CancellationToken, LocalExecutor,
    };
    static STATUS: AtomicU32 = AtomicU32::new(0);
    struct State(Rc<Cell<u32>>);
    impl Actor for State {}
    struct Add(Rc<u32>);
    impl Message for Add {
        type Reply = Rc<u32>;
    }
    impl Handler<Add> for State {
        type Future<'a> = BoxFuture<'a, Rc<u32>>;
        fn handle<'a>(&'a mut self, message: Add, _: ActorContext) -> Self::Future<'a> {
            Box::pin(async move {
                uniffi_core::async_runtime::sleep(Duration::ZERO).await;
                self.0.set(self.0.get() + *message.0);
                Rc::new(self.0.get())
            })
        }
    }
    struct Hold {
        started: oneshot::Sender<()>,
        gate: oneshot::Receiver<()>,
    }
    impl Message for Hold {
        type Reply = ();
    }
    impl Handler<Hold> for State {
        type Future<'a> = BoxFuture<'a, ()>;
        fn handle<'a>(&'a mut self, message: Hold, ctx: ActorContext) -> Self::Future<'a> {
            Box::pin(async move {
                let local = Rc::new(());
                current_context()
                    .unwrap()
                    .defer_cleanup(async move {
                        message.gate.await.unwrap();
                        drop(local);
                    })
                    .unwrap();
                message.started.send(()).unwrap();
                ctx.cancelled().await;
                self.0.set(40);
            })
        }
    }
    async fn smoke() {
        let (owner, address) = start(State(Rc::new(Cell::new(0))), 2).unwrap();
        assert_eq!(*address.ask(Add(Rc::new(3))).await.unwrap(), 3);
        let context = CallContext::new(CancellationToken::new());
        let (started, ready) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let mut call = Box::pin(context.run(address.ask(Hold { started, gate })));
        assert!(poll!(&mut call).is_pending());
        ready.await.unwrap();
        drop(call);
        let mut cleanup = Box::pin(context.wait_closed());
        assert!(poll!(&mut cleanup).is_pending());
        release.send(()).unwrap();
        cleanup.await.unwrap();
        assert_eq!(*address.ask(Add(Rc::new(2))).await.unwrap(), 42);
        address
            .notify_after(Duration::from_secs(3600), Add(Rc::new(99)))
            .unwrap();
        owner.drain_and_close().await.unwrap();
        let executor = LocalExecutor::new(4).unwrap();
        let local = Rc::new(Cell::new(10));
        let (owner, address) = start_local(&executor, move || State(local), 1)
            .await
            .unwrap();
        assert_eq!(*address.ask(Add(Rc::new(2))).await.unwrap(), 12);
        owner.close().await.unwrap();
        executor.close().await.unwrap();
        STATUS.store(1, Ordering::SeqCst);
    }
    #[no_mangle]
    pub extern "C" fn actor_smoke_start() {
        wasm_bindgen_futures::spawn_local(smoke());
    }
    #[no_mangle]
    pub extern "C" fn actor_smoke_status() -> u32 {
        STATUS.load(Ordering::SeqCst)
    }
}
