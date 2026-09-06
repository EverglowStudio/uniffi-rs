//! Opt-in native diagnostic, not a cross-language performance acceptance test.
//! Build first, then run `local_cost direct|local|local-context [batches] [iterations]`.
//! RSS is sampled outside timing on platforms providing the `ps` command.

use std::{cell::Cell, hint::black_box, rc::Rc, time::Instant};
use uniffi_core::async_runtime::{CallContext, CancellationToken, LocalExecutor};

struct Counter(Rc<Cell<u64>>);
impl Counter {
    async fn increment(&self) -> u64 {
        self.0.set(black_box(self.0.get()).wrapping_add(1));
        black_box(self.0.get())
    }
}

fn rss_kib() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("local");
    assert!(matches!(mode, "direct" | "local" | "local-context"));
    let batches: usize = args.get(2).map(|v| v.parse().unwrap()).unwrap_or(30);
    let iterations: usize = args.get(3).map(|v| v.parse().unwrap()).unwrap_or(2000);
    assert!(batches > 0 && iterations > 0);
    futures::executor::block_on(async {
        let direct = Counter(Rc::new(Cell::new(0)));
        let owner = LocalExecutor::new(8).unwrap();
        let local = owner
            .create(|| async { Counter(Rc::new(Cell::new(0))) })
            .await
            .unwrap();
        let call = || async {
            match mode {
                "direct" => direct.increment().await,
                "local" => local.call(|v| Box::pin(v.increment())).await.unwrap(),
                "local-context" => {
                    let context = CallContext::new(CancellationToken::new());
                    context
                        .run(local.call(|v| Box::pin(v.increment())))
                        .await
                        .unwrap()
                        .unwrap()
                }
                _ => unreachable!(),
            }
        };
        for _ in 0..1000 {
            black_box(call().await);
        }
        let before = rss_kib();
        let mut samples = vec![0u64; iterations];
        for batch in 0..batches {
            let start = Instant::now();
            for sample in &mut samples {
                let tick = Instant::now();
                black_box(call().await);
                *sample = tick.elapsed().as_nanos() as u64;
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.sort_unstable();
            println!("{{\"mode\":\"{mode}\",\"batch\":{batch},\"iterations\":{iterations},\"elapsed_ns\":{elapsed},\"p50_ns\":{},\"p95_ns\":{}}}",
                samples[iterations/2], samples[(iterations*95/100).min(iterations-1)]);
        }
        local.close().await.unwrap();
        owner.close().await.unwrap();
        let after = rss_kib();
        println!("{{\"mode\":\"{mode}\",\"rss_before_kib\":{},\"rss_after_kib\":{},\"status\":\"complete\"}}",
            before.map(|v|v.to_string()).unwrap_or("null".into()),
            after.map(|v|v.to_string()).unwrap_or("null".into()));
    });
}
