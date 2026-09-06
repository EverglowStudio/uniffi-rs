# Existing foreign async baseline

This independent harness supplements the synchronous Criterion suite. It changes
benchmark fixtures: there is no Tokio runtime, timer delay, new backend
capability, or core optimization. S00 also repairs Kotlin template initialization
accesses and Unit lift lambdas for strict Kotlin 2.4 compilation, retaining their side effects. Use the
same calibrated templates for A/B. `async-trait` reuses the workspace's
locked version (the benchmark package's lock dependency list gains one entry).

| Workload | Swift | Kotlin JNA | Kotlin JNI |
| --- | --- | --- | --- |
| Ready Future / pending Future with worker wake | yes | yes | yes |
| Rust → foreign async callback, ready / host yield | yes | yes | yes |
| Pending Future cancellation through Rust drop | yes | yes | absent in baseline |
| Output Stream, ready / worker wake per item | yes | yes | absent in baseline |
| Input Stream / input-to-output roundtrip | yes | yes | absent in baseline |

The pending Future always returns `Pending` on its first poll; a persistent Rust
worker receives its waker and wakes the host. The worker is created during warmup.
Calls run sequentially, so there is at most one submitted wake per operation.
After sampling the fixture stops and joins its worker. JNI teardown also detaches
that native worker from the JVM after all wake callbacks return, as required by
the [JNI invocation contract](https://docs.oracle.com/en/java/javase/23/docs/specs/jni/invocation.html#detaching-from-the-vm).
This process teardown is outside operation timings; it does not alter the existing
JNI attach/wake behavior measured in each sample.
Callbacks are measured as a full host → Rust → host → Rust → host roundtrip;
`callback_yield` additionally calls the host's yield primitive.

JNI uses `suspendCoroutine` without forwarding Job cancellation. A never-ready
Future therefore cannot complete `cancelAndJoin`; the calibration confirmed this.
The harness reports this as unsupported rather than inventing a cancellation
baseline or modifying the JNI runtime. No Stream ABI exists there either.

`future_cancel_lifecycle` includes object/task creation, waiting for the first Rust
poll, cancellation, joining the host task, checking Rust Drop, and object release.
It is **not** a pure cancel-to-drop latency. No cleanup is excluded from this case.
Stream operations include construction, all items, EOF and normal disposal. Every
output item is checked in order; input consumption checks the sum. Swift input is
a demand-driven AsyncSequence; JNA input is a cold Flow. Neither benchmark adds an
unbounded producer queue. Stream p95 is traversal latency, not the distribution of
individual `next` calls: use **both `--stream-items 1` and `64`** for single-item
latency and multi-item throughput, keeping workload size identical in A/B.

## Prepare (compilation permitted)

From the checkout under test, use an output directory unique to its revision and
backend. Cargo's target directory may be shared; do not copy the target tree.
Only generated bindings and final libraries/executables are staged in `--out`.
The following macOS commands use the S00 environment file:

```sh
python3 fixtures/benchmarks/async/run.py prepare swift \
  --environment /Users/frain/Developer/.tools/uniffi-async/environment.json \
  --target-dir /Users/frain/Developer/uniffi-rs/target \
  --out /Users/frain/Developer/.tools/uniffi-async/foreign-bench/swift
```

Repeat with `jna` and `jni` and separate output directories. Rust is built with
`--locked --release`; Swift bindings and the executable use `-O -swift-version 5`;
Kotlin uses `-Werror`, JVM target 17 and runs on the configured JDK, with 512 MiB initial/max
heap. `build.json` preserves exact features, commands, versions, product commit,
harness/template/JAR hashes, all artifact hashes, and environment. Source snapshots
and the tracked calibration patch are saved too. `Cargo.lock` is copied
alongside it. No independent fixture lock exists. Before formal sampling, record
the integrated harness commit in the S00 artifacts as well as these file hashes.

## Calibration and sampling (no compilers)

```sh
python3 fixtures/benchmarks/async/run.py sample swift \
  --environment /Users/frain/Developer/.tools/uniffi-async/environment.json \
  --out /Users/frain/Developer/.tools/uniffi-async/foreign-bench/swift \
  --samples 3 --iterations 10 --warmup 20 --stream-items 64 --label calibration
```

Use the same command with `jna` / `jni`. Record stdout/stderr in separate uniquely
named `foreign-bench-*.log` files in the requirement's `artifacts/S00/` directory.
`sample` validates staged hashes and never invokes Cargo, a compiler, or generator.
The process has a 30-minute watchdog; cancellation handshakes have 10-second limits.

After other compilation ends, use at least `--samples 30 --iterations 100
--warmup 1000`, in at least three alternating A/B groups per backend and workload
size. Use `--case` to isolate a noisy case. The first Rust call initialization is
reported separately; each case then warms up before timing. JVM process startup,
class loading during first use and warmup are outside samples. Every JSON sample
contains all operation durations and batch wall time; throughput includes loop and
clock overhead, while logging occurs after the timer stops. No samples are silently
removed, and failed value or lifecycle checks terminate the run.

```sh
python3 fixtures/benchmarks/async/summarize.py /absolute/path/to/foreign-bench-run.log
```

The summary reports mean/p95 operation latency, operations/items per second, and
95% intervals using deterministic whole-batch bootstrap resampling. Keep raw logs.
Calibration runs cannot establish the 5% gate. Compare each backend/case/size
separately; overlapping a gate requires quieter reruns, not a pass declaration.
