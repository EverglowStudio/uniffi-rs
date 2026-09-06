// This driver is copied beside the generated release bindings by benchmark.rs.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { cpus } from "node:os";
import { performance } from "node:perf_hooks";
import { setImmediate as nextTurn } from "node:timers/promises";

function positiveInteger(name: string, fallback: string): number {
  const value = Number(process.env[name] ?? fallback);
  assert(Number.isSafeInteger(value) && value > 0, `${name} must be a positive safe integer`);
  return value;
}

const iterations = positiveInteger("UNIFFI_JS_BENCH_ITERS", "200");
const samples = positiveInteger("UNIFFI_JS_BENCH_SAMPLES", "30");
const streamRepetitions = positiveInteger("UNIFFI_JS_STREAM_BENCH_REPS", "3");
const streamCounts = (process.env.UNIFFI_JS_STREAM_BENCH_COUNTS ?? "1,100,1000,10000")
  .split(",").map((value) => Number(value.trim()));
assert(streamCounts.length > 0 && streamCounts.every((n) => Number.isSafeInteger(n) && n > 0 && n <= 0xffffffff),
  "UNIFFI_JS_STREAM_BENCH_COUNTS must contain positive u32 counts");
assert.equal(new Set(streamCounts).size, streamCounts.length, "stream counts must be unique");
const group = process.env.UNIFFI_JS_BENCH_GROUP ?? "unspecified";
const build = JSON.parse(readFileSync(new URL("./benchmark-build.json", import.meta.url), "utf8"));
assert.equal(build.buildExitCode, 0, "both release hosts must build successfully before measurement");
const meta = {
  ...build, node: process.version, platform: process.platform, arch: process.arch,
  cpu: cpus()[0].model, iterations, samples, streamRepetitions, streamCounts,
  mode: samples === 1 ? "calibration" : "measurement", group,
  timestamp: new Date().toISOString(), execArgv: process.execArgv,
  timingDefinitions: {
    sync: "Batch elapsed time includes calls and assertions; object-method also creates/disposes one object per operation.",
    async: "latenciesMs measures each awaited call including assertions; batch elapsed time also includes timer/sample collection overhead.",
    stream: "latenciesMs measures each complete traversal: creation of input/output streams, all items, EOF and completion-path release, plus assertions. msPerItem/msPerOp is amortized traversal cost per item, never individual next latency or next p95.",
    hostWake: "Each latency runs from wake() through awaited completion; excludes registration, object creation/disposal and assertions. Reciprocal latency is not sustained throughput.",
    streamReturnEarly: "Each latency runs from return() through awaited terminal result, including native cancellation/release. Creation, first-item read and terminal/Drop/idempotency assertions are outside timing; one of two items remains unread.",
  },
  unsupportedBaselines: {
    promisePerCallCancellation: "No cancellation entrypoint on the existing ordinary Promise API; no old per-call cancellation baseline.",
    perStreamClose: "No close() method on a subscription; return() is measured. Root/session close() is session teardown, excluded from steady-state subscription timings.",
  },
};
console.log(JSON.stringify({ ...meta, case: "metadata" }));

function report(backend: string, caseName: string, sample: number, operations: number, elapsedMs: number, extra = {}) {
  assert(Number.isFinite(elapsedMs) && elapsedMs > 0, `${backend}/${caseName}: invalid timing`);
  console.log(JSON.stringify({
    backend, case: caseName, group, sample, operations, elapsedMs,
    msPerOp: elapsedMs / operations, opsPerSec: operations * 1000 / elapsedMs, ...extra,
  }));
}

function bench(backend: string, caseName: string, fn: () => void) {
  for (let i = 0; i < Math.min(iterations, 10); i += 1) fn();
  for (let sample = 0; sample < samples; sample += 1) {
    const start = performance.now();
    for (let i = 0; i < iterations; i += 1) fn();
    report(backend, caseName, sample, iterations, performance.now() - start);
  }
}

async function benchAsync(backend: string, caseName: string, fn: () => Promise<void>) {
  for (let i = 0; i < Math.min(iterations, 10); i += 1) await fn();
  for (let sample = 0; sample < samples; sample += 1) {
    const latenciesMs: number[] = [];
    const start = performance.now();
    for (let i = 0; i < iterations; i += 1) {
      const operationStart = performance.now();
      await fn();
      latenciesMs.push(performance.now() - operationStart);
    }
    report(backend, caseName, sample, iterations, performance.now() - start, { latenciesMs });
  }
}

async function benchStream(backend: string, caseName: string, count: number, fn: (n: number) => Promise<void>) {
  await fn(count);
  for (let sample = 0; sample < samples; sample += 1) {
    const latenciesMs: number[] = [];
    const start = performance.now();
    for (let i = 0; i < streamRepetitions; i += 1) {
      const traversalStart = performance.now();
      await fn(count);
      latenciesMs.push(performance.now() - traversalStart);
    }
    const elapsedMs = performance.now() - start;
    const items = count * streamRepetitions;
    report(backend, caseName, sample, items, elapsedMs, {
      count, repetitions: streamRepetitions, items, msPerItem: elapsedMs / items,
      itemsPerSec: items * 1000 / elapsedMs,
      latenciesMs, latencyUnit: "full-traversal", operationUnit: "item",
    });
  }
}

function expectedSum(count: number): bigint {
  return (BigInt(count) * BigInt(count - 1)) / 2n;
}

async function* makeStreamItems(count: number) {
  for (let i = 0; i < count; i += 1) yield { value: i };
}

// Check every item: length/last-value checks alone miss corruption in the middle.
async function consumeOutputStream(stream: AsyncIterable<{ value: number }>, count: number) {
  let seen = 0;
  for await (const item of stream) {
    assert.equal(item.value, seen);
    seen += 1;
  }
  assert.equal(seen, count);
}

async function consumeInputStream(core, count: number) {
  assert.equal(await core.sumInputStream(makeStreamItems(count)), expectedSum(count));
}

async function consumeBidiStream(core, count: number) {
  let seen = 0;
  let sum = 0;
  for await (const item of core.runningSumStream(makeStreamItems(count))) {
    sum = (sum + seen) >>> 0; // Rust running sum deliberately wraps u32.
    assert.equal(item.value, sum);
    seen += 1;
  }
  assert.equal(seen, count);
}

async function wakeOnce(core): Promise<number> {
  const gate = core.BenchWake.new();
  const completion = gate.wait();
  try {
    const deadline = performance.now() + 10_000;
    while (!gate.isWaiting()) {
      assert(performance.now() < deadline, "Rust Future did not register its waker");
      await nextTurn();
    }
    // Registration and object creation are excluded; JS -> Rust wake -> JS
    // completion is included. This is a latency sample, not sustained throughput.
    const start = performance.now();
    gate.wake();
    const value = await completion;
    const elapsedMs = performance.now() - start;
    assert.equal(value, 42);
    assert.equal(gate.isWaiting(), false);
    return elapsedMs;
  } finally {
    gate.dispose();
  }
}

async function benchWake(label: string, core) {
  for (let i = 0; i < Math.min(iterations, 10); i += 1) await wakeOnce(core);
  for (let sample = 0; sample < samples; sample += 1) {
    const latenciesMs: number[] = [];
    for (let i = 0; i < iterations; i += 1) latenciesMs.push(await wakeOnce(core));
    const elapsedMs = latenciesMs.reduce((sum, n) => sum + n, 0);
    console.log(JSON.stringify({ backend: label, case: "host-wake", group, sample, operations: iterations,
      elapsedMs, msPerOp: elapsedMs / iterations, latenciesMs }));
  }
}

async function returnStreamOnce(core): Promise<number> {
  const drops = core.earlyExitStreamDrops();
  const stream = core.earlyExitStream();
  assert.deepEqual(await stream.next(), { done: false, value: { value: 0 } });
  assert.equal(core.earlyExitStreamDrops(), drops);
  const start = performance.now();
  const terminal = await stream.return();
  const elapsedMs = performance.now() - start;
  assert.deepEqual(terminal, { done: true, value: undefined });
  assert.equal(core.earlyExitStreamDrops(), drops + 1);
  assert.deepEqual(await stream.next(), { done: true, value: undefined });
  assert.deepEqual(await stream.return(), { done: true, value: undefined });
  assert.equal(core.earlyExitStreamDrops(), drops + 1);
  return elapsedMs;
}

async function benchStreamReturn(label: string, core) {
  for (let i = 0; i < Math.min(iterations, 10); i += 1) await returnStreamOnce(core);
  for (let sample = 0; sample < samples; sample += 1) {
    const latenciesMs: number[] = [];
    for (let i = 0; i < iterations; i += 1) latenciesMs.push(await returnStreamOnce(core));
    const elapsedMs = latenciesMs.reduce((sum, n) => sum + n, 0);
    console.log(JSON.stringify({ backend: label, case: "stream-return-early", group, sample,
      operations: iterations, elapsedMs, msPerOp: elapsedMs / iterations, latenciesMs,
      latencyUnit: "return-to-terminal", operationUnit: "subscription",
      availableItems: 2, consumedItems: 1 }));
  }
}

async function benchBackend(label: string, core) {
  const record = { count: 7, label: "record" };
  const enumValue = { tag: "One", value: 9 };
  const counts = new Map([["a", 1], ["b", 2]]);
  const nested = { record, enumValue, values: [1, 2, 3], counts };
  const largeString = "x".repeat(4096);
  const callback = { bump(value: number) { return value + 1; } };

  // Untimed boundary calibration uses the same APIs and validators.
  assert.deepEqual(core.mapRoundtrip(new Map()), new Map());
  assert.deepEqual(core.enumRoundtrip({ tag: "Two", label: "two" }), { tag: "Two", label: "two" });
  for (const count of [0, 1]) {
    await consumeOutputStream(core.countStream(count), count);
    await consumeOutputStream(core.pendingCountStream(count), count);
    await consumeInputStream(core, count);
    await consumeBidiStream(core, count);
  }
  bench(label, "scalar-u64-add", () => assert.equal(core.add(2n, 3n), 5n));
  bench(label, "string-concat", () => assert.equal(core.concat("a", "b"), "ab"));
  bench(label, "large-string-roundtrip", () => assert.equal(core.largeStringRoundtrip(largeString), largeString));
  bench(label, "record-roundtrip", () => assert.deepEqual(core.recordRoundtrip(record), record));
  bench(label, "enum-roundtrip", () => assert.deepEqual(core.enumRoundtrip(enumValue), enumValue));
  bench(label, "vec-roundtrip", () => assert.deepEqual(core.vecRoundtrip([1, 2, 3]), [1, 2, 3]));
  bench(label, "map-roundtrip", () => assert.deepEqual(core.mapRoundtrip(new Map([["a", 1], ["b", 2]])), counts));
  bench(label, "nested-data-roundtrip", () => assert.deepEqual(core.nestedDataRoundtrip(nested), nested));
  bench(label, "object-method", () => {
    const counter = core.BenchCounter.new(1);
    try { assert.equal(counter.increment(), 2); } finally { counter.dispose(); }
  });
  bench(label, "sync-callback", () => assert.equal(core.callCallback(callback, 4), 5));
  await benchAsync(label, "async-u64-add", async () => { assert.equal(await core.asyncAdd(2n, 3n), 5n); });
  await benchAsync(label, "async-pending-add", async () => { assert.equal(await core.pendingAdd(2n, 3n), 5n); });
  for (const yieldOnce of [false, true]) {
    const caseName = yieldOnce ? "async-callback-yield" : "async-callback-ready";
    let calls = 0;
    let completions = 0;
    let yields = 0;
    const asyncCallback = {
      async bump(value: number): Promise<number> {
        calls += 1;
        assert.equal(value, 4);
        if (yieldOnce) {
          // Yield to a new host event-loop turn, not just an already-resolved Promise.
          await nextTurn();
          yields += 1;
        }
        completions += 1;
        return value + 1;
      },
    };
    await benchAsync(label, caseName, async () => {
      const before = calls;
      assert.equal(await core.callAsyncCallback(asyncCallback, 4), 5);
      assert.equal(calls, before + 1);
      assert.equal(completions, calls);
      assert.equal(yields, yieldOnce ? calls : 0);
    });
    assert.equal(calls, Math.min(iterations, 10) + samples * iterations);
  }
  await benchWake(label, core);
  await benchStreamReturn(label, core);
  for (const count of streamCounts) {
    await benchStream(label, "stream-output", count, async (n) => consumeOutputStream(core.countStream(n), n));
    await benchStream(label, "stream-output-pending", count, async (n) => consumeOutputStream(core.pendingCountStream(n), n));
    await benchStream(label, "stream-input", count, async (n) => consumeInputStream(core, n));
    await benchStream(label, "stream-bidi", count, async (n) => consumeBidiStream(core, n));
  }
}

// The watchdog also makes a broken wake/Stream fail instead of hanging forever.
const watchdog = setTimeout(() => {
  console.error("benchmark timed out after 10 minutes");
  process.exit(1);
}, 600_000);
try {
  let start = performance.now();
  const nodeRoot = await import("./generated/node/index.js");
  console.log(JSON.stringify({ backend: "napi", case: "initialization", elapsedMs: performance.now() - start }));
  try { await benchBackend("napi", nodeRoot.js_benchmark_fixture); } finally { await nodeRoot.close(); }
  start = performance.now();
  const browserRoot = await import("./generated/browser/index.js");
  await browserRoot.ready;
  console.log(JSON.stringify({ backend: "wasm", case: "initialization", elapsedMs: performance.now() - start }));
  try { await benchBackend("wasm", browserRoot.js_benchmark_fixture); } finally { await browserRoot.close(); }
} finally {
  clearTimeout(watchdog);
}
