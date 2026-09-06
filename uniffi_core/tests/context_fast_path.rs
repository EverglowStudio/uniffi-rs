//! Native, allocation-inclusive microbench (no cancellation or deferred cleanup).
//! Run with:
//! `cargo test -p uniffi_core --features async-runtime --release --test context_fast_path -- --ignored --nocapture --test-threads=1`
//! Each JSON line measures fresh token/context creation, run, completion and drop.
//! Executor overhead is included; compare identical release builds on the same host.

#![cfg(all(feature = "async-runtime", not(target_arch = "wasm32")))]

use futures::{executor::block_on, future::poll_fn};
use std::{hint::black_box, task::Poll, time::Instant};
use uniffi_core::async_runtime::{CallContext, CancellationToken};

const LOOPS: usize = 100_000;
const BATCHES: usize = 5;
const WARMUP: usize = 10_000;
const VALUE: u64 = 7;

fn ready_call() -> u64 {
    let token = black_box(CancellationToken::new());
    let context = black_box(CallContext::new(token));
    let future = std::future::ready(black_box(VALUE));
    let value = black_box(block_on(context.run(future))).expect("uncancelled call");
    // block_on owns and drops Run; this drops the final context/token owner.
    drop(context);
    value
}

fn pending_once_call() -> u64 {
    let token = black_box(CancellationToken::new());
    let context = black_box(CallContext::new(token));
    let value = black_box(VALUE);
    let mut pending = true;
    let future = poll_fn(move |cx| {
        if pending {
            pending = false;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(value)
        }
    });
    let value = black_box(block_on(context.run(future))).expect("uncancelled call");
    drop(context);
    value
}

fn measure(path: &str, batch: usize, mut call: impl FnMut() -> u64) {
    let mut checksum = 0;
    let start = Instant::now();
    for _ in 0..LOOPS {
        checksum += black_box(call());
    }
    let elapsed = start.elapsed();
    assert_eq!(checksum, VALUE * LOOPS as u64);
    println!(
        "{{\"benchmark\":\"context_fast_path\",\"path\":\"{path}\",\"batch\":{batch},\"loops\":{LOOPS},\"warmup_loops\":{WARMUP},\"elapsed_ns\":{},\"ns_per_call\":{:.3},\"checksum\":{checksum}}}",
        elapsed.as_nanos(),
        elapsed.as_nanos() as f64 / LOOPS as f64,
    );
}

#[test]
#[ignore = "manual release microbench; prints JSON timings, no performance threshold"]
fn context_fast_path() {
    assert!(
        !cfg!(debug_assertions),
        "run this microbench with --release"
    );
    for _ in 0..WARMUP {
        assert_eq!(black_box(ready_call()), VALUE);
        assert_eq!(black_box(pending_once_call()), VALUE);
    }
    // Alternate path order to reduce systematic warmup/order bias.
    for batch in 1..=BATCHES {
        if batch % 2 == 1 {
            measure("ready", batch, ready_call);
            measure("pending_once_self_wake", batch, pending_once_call);
        } else {
            measure("pending_once_self_wake", batch, pending_once_call);
            measure("ready", batch, ready_call);
        }
    }
}
