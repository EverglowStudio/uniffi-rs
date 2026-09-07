//! Reproducible native actor diagnostics. Build before any coordinated sampling.
//! Usage: actor_cost send|local ask|tell|full|full-wait|close|drain|memory
//!        [batches=20] [iterations=1000] [capacity=64] [payload-bytes=65536]
//! Send uses a current-thread Tokio runtime; local additionally uses one S02
//! owner thread. Timing includes public API overhead, not FFI or an old actor.
use futures::{channel::oneshot, poll};
use std::{
    cell::Cell,
    hint::black_box,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};
use uniffi_core::async_runtime::{actor::*, BoxFuture, LocalBoxFuture, LocalExecutor};
trait Value: 'static {
    fn get(&self) -> u64;
    fn set(&self, value: u64);
}
impl Value for Cell<u64> {
    fn get(&self) -> u64 {
        Cell::get(self)
    }
    fn set(&self, value: u64) {
        Cell::set(self, value);
    }
}
impl Value for Rc<Cell<u64>> {
    fn get(&self) -> u64 {
        self.as_ref().get()
    }
    fn set(&self, value: u64) {
        self.as_ref().set(value);
    }
}
struct Counter<S: Value>(S);
impl<S: Value> Actor for Counter<S> {}
#[derive(Default)]
struct Retained {
    live: AtomicUsize,
    peak: AtomicUsize,
    dropped: AtomicUsize,
}
struct Payload {
    bytes: Vec<u8>,
    retained: Arc<Retained>,
}
impl Payload {
    fn new(bytes: usize, retained: &Arc<Retained>) -> Self {
        let live = retained.live.fetch_add(1, Ordering::SeqCst) + 1;
        retained.peak.fetch_max(live, Ordering::SeqCst);
        Self {
            bytes: vec![0xa5; bytes],
            retained: retained.clone(),
        }
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.retained.live.fetch_sub(1, Ordering::SeqCst);
        self.retained.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
enum Op {
    Increment,
    Gate {
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    Payload(Payload),
}
impl Message for Op {
    type Reply = u64;
}
impl<S: Value> Counter<S> {
    async fn apply(&mut self, op: Op, ctx: ActorContext) -> u64 {
        match op {
            Op::Increment => self.0.set(black_box(self.0.get()).wrapping_add(1)),
            Op::Gate { started, release } => {
                started.send(()).unwrap();
                let cancelled = ctx.cancelled();
                futures::pin_mut!(cancelled, release);
                let _ = futures::future::select(cancelled, release).await;
            }
            Op::Payload(payload) => {
                black_box(payload.bytes.first());
            }
        }
        black_box(self.0.get())
    }
}
impl Handler<Op> for Counter<Cell<u64>> {
    type Future<'a> = BoxFuture<'a, u64>;
    fn handle<'a>(&'a mut self, op: Op, ctx: ActorContext) -> Self::Future<'a> {
        Box::pin(self.apply(op, ctx))
    }
}
impl Handler<Op> for Counter<Rc<Cell<u64>>> {
    type Future<'a> = LocalBoxFuture<'a, u64>;
    fn handle<'a>(&'a mut self, op: Op, ctx: ActorContext) -> Self::Future<'a> {
        Box::pin(self.apply(op, ctx))
    }
}
enum Address {
    Send(ActorRef<Counter<Cell<u64>>>),
    Local(LocalActorRef<Counter<Rc<Cell<u64>>>>),
}
impl Address {
    fn ask(&self, op: Op) -> Ask<u64> {
        match self {
            Self::Send(a) => a.ask(op),
            Self::Local(a) => a.ask(op),
        }
    }
    async fn tell(&self, op: Op) -> Result<(), ActorError> {
        match self {
            Self::Send(a) => a.tell(op).await,
            Self::Local(a) => a.tell(op).await,
        }
    }
    fn try_send(&self, op: Op) -> Result<(), TrySendError<Op>> {
        match self {
            Self::Send(a) => a.try_send(op),
            Self::Local(a) => a.try_send(op),
        }
    }
}
async fn create(executor: Option<&LocalExecutor>, capacity: usize) -> (ActorOwner, Address) {
    if let Some(executor) = executor {
        let (owner, address) = start_local(executor, || Counter(Rc::new(Cell::new(0))), capacity)
            .await
            .unwrap();
        (owner, Address::Local(address))
    } else {
        let (owner, address) = start(Counter(Cell::new(0)), capacity).unwrap();
        (owner, Address::Send(address))
    }
}
async fn fill(address: &Address, capacity: usize) -> oneshot::Sender<()> {
    let (started, ready) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    address
        .tell(Op::Gate {
            started,
            release: gate,
        })
        .await
        .unwrap();
    ready.await.unwrap();
    for _ in 0..capacity {
        address.tell(Op::Increment).await.unwrap();
    }
    release
}
fn rss_kib() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}
fn json_option(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}
async fn measure(
    executor: Option<&LocalExecutor>,
    case: &str,
    capacity: usize,
    address: &Address,
) -> u64 {
    match case {
        "ask" => {
            let tick = Instant::now();
            black_box(address.ask(Op::Increment).await.unwrap());
            tick.elapsed().as_nanos() as u64
        }
        "tell" => {
            let tick = Instant::now();
            address.tell(Op::Increment).await.unwrap();
            tick.elapsed().as_nanos() as u64
        }
        "full" => {
            let tick = Instant::now();
            assert!(matches!(
                address.try_send(Op::Increment),
                Err(TrySendError::Full(_))
            ));
            tick.elapsed().as_nanos() as u64
        }
        "full-wait" | "close" | "drain" => {
            let (owner, address) = create(executor, capacity).await;
            let release = fill(&address, capacity).await;
            if case == "full-wait" {
                let mut waiting = Box::pin(address.tell(Op::Increment));
                assert!(poll!(&mut waiting).is_pending());
                let tick = Instant::now();
                release.send(()).unwrap();
                waiting.await.unwrap();
                let ns = tick.elapsed().as_nanos() as u64;
                owner.drain_and_close().await.unwrap();
                ns
            } else {
                let tick = Instant::now();
                if case == "close" {
                    owner.close().await.unwrap();
                    drop(release);
                } else {
                    release.send(()).unwrap();
                    owner.drain_and_close().await.unwrap();
                }
                tick.elapsed().as_nanos() as u64
            }
        }
        _ => unreachable!(),
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let backend = args.get(1).map(String::as_str).unwrap_or("send");
    let case = args.get(2).map(String::as_str).unwrap_or("ask");
    assert!(matches!(backend, "send" | "local"));
    assert!(matches!(
        case,
        "ask" | "tell" | "full" | "full-wait" | "close" | "drain" | "memory"
    ));
    let number = |i, default| {
        args.get(i)
            .map(|v: &String| v.parse::<usize>().unwrap())
            .unwrap_or(default)
    };
    let batches = number(3, 20);
    let iterations = number(4, 1000);
    let capacity = number(5, 64);
    let payload_bytes = number(6, 65536);
    assert!(batches > 0 && iterations > 0 && capacity > 0 && payload_bytes > 0);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async{
        let executor=if backend=="local"{Some(LocalExecutor::new(8).unwrap())}else{None};
        let (owner,address)=create(executor.as_ref(),capacity).await;
        if case=="memory"{
            let retained=Arc::new(Retained::default());let (started,ready)=oneshot::channel();let (_release,gate)=oneshot::channel();
            address.tell(Op::Gate{started,release:gate}).await.unwrap();ready.await.unwrap();let before=rss_kib();
            for _ in 0..capacity{address.tell(Op::Payload(Payload::new(payload_bytes,&retained))).await.unwrap();}
            let full=matches!(address.try_send(Op::Payload(Payload::new(payload_bytes,&retained))),Err(TrySendError::Full(_)));assert!(full);
            assert_eq!(retained.live.load(Ordering::SeqCst),capacity);
            let full_rss=rss_kib();let live=retained.live.load(Ordering::SeqCst);let peak=retained.peak.load(Ordering::SeqCst);
            owner.close().await.unwrap();let after=rss_kib();assert_eq!(retained.live.load(Ordering::SeqCst),0);
            println!("{{\"backend\":\"{backend}\",\"case\":\"memory\",\"capacity\":{capacity},\"payload_bytes\":{payload_bytes},\"accepted_live\":{live},\"peak_including_rejected_caller\":{peak},\"live_after_close\":0,\"dropped\":{},\"rss_before_kib\":{},\"rss_full_kib\":{},\"rss_after_kib\":{},\"status\":\"complete\"}}",retained.dropped.load(Ordering::SeqCst),json_option(before),json_option(full_rss),json_option(after));
        }else{
            let full_gate=if case=="full"{Some(fill(&address,capacity).await)}else{None};
            for _ in 0..if matches!(case,"ask"|"tell"|"full"){100}else{5}{black_box(measure(executor.as_ref(),case,capacity,&address).await);}
            let before=rss_kib();let mut samples=vec![0u64;iterations];
            for batch in 0..batches{
                for sample in &mut samples{*sample=measure(executor.as_ref(),case,capacity,&address).await;}
                let mut sorted=samples.clone();sorted.sort_unstable();
                let total:u64=samples.iter().sum();
                println!("{{\"backend\":\"{backend}\",\"case\":\"{case}\",\"batch\":{batch},\"capacity\":{capacity},\"iterations\":{iterations},\"sum_operation_ns\":{total},\"p50_ns\":{},\"p95_ns\":{},\"samples_ns\":{:?}}}",sorted[iterations/2],sorted[(iterations*95/100).min(iterations-1)],samples);
            }
            if case=="full"{owner.close().await.unwrap();drop(full_gate);}else{owner.drain_and_close().await.unwrap();}
            println!("{{\"backend\":\"{backend}\",\"case\":\"{case}\",\"rss_before_kib\":{},\"rss_after_kib\":{},\"status\":\"complete\"}}",json_option(before),json_option(rss_kib()));
        }
        if let Some(executor)=executor{executor.close().await.unwrap();}
    });
}
