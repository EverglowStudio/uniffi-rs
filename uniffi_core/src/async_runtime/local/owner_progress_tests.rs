//! Drive the real owner with a controlled caller, without timing assumptions.

use super::*;
use futures_util::task::{waker, ArcWake};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Wakes(AtomicUsize);

impl ArcWake for Wakes {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn owner_channel(capacity: usize) -> (Arc<Control>, mpsc::Sender<Packet>, mpsc::Receiver<Packet>) {
    let control = Arc::new(Control {
        executor: Mutex::new(std::sync::Weak::new()),
        stop: CancellationToken::new(),
        changed: Notify::new(),
        done: Completion::new(),
    });
    let (sender, receiver) = mpsc::channel(capacity);
    (control, sender, receiver)
}

fn packet(command: impl FnOnce(Rc<Owner>) + Send + 'static) -> Packet {
    Packet {
        command: Some(Box::new(command)),
        done: Completion::new(),
    }
}

fn close_owner(
    control: &Control,
    mut driver: Pin<&mut impl Future<Output = ()>>,
    cx: &mut Context<'_>,
) {
    control.stop.cancel();
    control.changed.notify_one();
    for _ in 0..64 {
        if driver.as_mut().poll(cx).is_ready() {
            return;
        }
    }
    panic!("owner did not finish its stopped tasks");
}

fn enqueue_pending_chain(owner: Rc<Owner>, remaining: usize, started: Arc<AtomicUsize>) {
    let next_owner = owner.clone();
    owner.push(Box::pin(async move {
        started.fetch_add(1, Ordering::SeqCst);
        if remaining > 1 {
            // Interleave a Ready task: it must not replenish an already spent
            // inline allowance and let the Pending chain run without returning.
            next_owner.push(Box::pin(async {}));
            enqueue_pending_chain(next_owner.clone(), remaining - 1, started);
        }
        next_owner.control.stop.cancelled().await;
    }));
}

#[test]
fn pending_spawn_chain_advances_inline_but_still_returns_to_caller() {
    const LINKS: usize = 32;
    let (control, sender, receiver) = owner_channel(1);
    let started = Arc::new(AtomicUsize::new(0));
    let observed = started.clone();
    sender
        .try_send(packet(move |owner| {
            enqueue_pending_chain(owner, LINKS, observed)
        }))
        .unwrap_or_else(|_| panic!("initial packet must fit"));
    let wakes = Arc::new(Wakes::default());
    let waker = waker(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut driver = Box::pin(drive(control.clone(), receiver));

    assert!(driver.as_mut().poll(&mut cx).is_pending());
    let first = started.load(Ordering::SeqCst);
    assert!(
        first > 1,
        "new owner-local work was deferred to an outer poll"
    );
    assert!(
        first < LINKS,
        "the Pending chain swallowed all outer yields"
    );
    for _ in 0..LINKS * 4 {
        if started.load(Ordering::SeqCst) == LINKS {
            break;
        }
        assert!(driver.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(started.load(Ordering::SeqCst), LINKS);
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    close_owner(&control, driver.as_mut(), &mut cx);
}

#[test]
fn idle_owner_does_not_add_self_wakes() {
    for with_waiting_task in [false, true] {
        let (control, sender, receiver) = owner_channel(1);
        if with_waiting_task {
            sender
                .try_send(packet(|owner| {
                    let control = owner.control.clone();
                    owner.push(Box::pin(async move { control.stop.cancelled().await }));
                }))
                .unwrap_or_else(|_| panic!("initial packet must fit"));
        }
        let wakes = Arc::new(Wakes::default());
        let waker = waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut driver = Box::pin(drive(control.clone(), receiver));
        assert!(driver.as_mut().poll(&mut cx).is_pending());
        // Initial task admission and FuturesUnordered's first pass can wake
        // the caller. Once waiting, neither an empty set nor a dormant task
        // should cause this owner to start self-waking without new work.
        wakes.0.store(0, Ordering::SeqCst);
        for _ in 0..4 {
            assert!(driver.as_mut().poll(&mut cx).is_pending());
            assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
        }
        close_owner(&control, driver.as_mut(), &mut cx);
    }
}

#[test]
fn ready_tasks_and_receiver_keep_draining_without_a_new_budget() {
    const PACKETS: usize = 16;
    let (control, sender, receiver) = owner_channel(PACKETS);
    let received = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    for _ in 0..PACKETS {
        let received = received.clone();
        let completed = completed.clone();
        sender
            .try_send(packet(move |owner| {
                received.fetch_add(1, Ordering::SeqCst);
                owner.push(Box::pin(async move {
                    completed.fetch_add(1, Ordering::SeqCst);
                }));
            }))
            .unwrap_or_else(|_| panic!("packet must fit"));
    }
    let waker = waker(Arc::new(Wakes::default()));
    let mut cx = Context::from_waker(&waker);
    let mut driver = Box::pin(drive(control.clone(), receiver));
    assert!(driver.as_mut().poll(&mut cx).is_pending());
    assert_eq!(received.load(Ordering::SeqCst), PACKETS);
    assert_eq!(completed.load(Ordering::SeqCst), PACKETS);
    close_owner(&control, driver.as_mut(), &mut cx);
}
