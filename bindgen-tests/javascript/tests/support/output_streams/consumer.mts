import { ready, output_streams_probe as api } from "./browser/index.js";
import type { UniFfiStream } from "./shared/uniffi_runtime.js";
import type { ProbeSnapshot } from "./components/output_streams_probe/index.js";
await ready;

function check(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
async function stable(id: string, expected: ProbeSnapshot): Promise<void> {
  for (let i = 0; i < 16; i++) {
    await Promise.resolve();
    const actual = api.probeSnapshot(id);
    for (const key of Object.keys(expected) as Array<keyof ProbeSnapshot>) {
      check(actual[key] === expected[key], `${id}: changed ${key}`);
    }
  }
  console.log(id, expected);
}
async function untilPolled(id: string): Promise<void> {
  for (let i = 0; i < 10000; i++) {
    if (api.probeSnapshot(id).streamNextPolls === 1n) return;
    await Promise.resolve();
  }
  throw new Error(`${id}: never polled`);
}

api.resetProbe("lazy");
const lazy: UniFfiStream<number> = api.countingStream("lazy", 2);
check(api.probeSnapshot("lazy").streamStarts === 0n, "wrapper is lazy");
check(lazy[Symbol.asyncIterator]() === lazy[Symbol.asyncIterator](), "one shared iterator");
check((await lazy.next()).value === 0, "first item");
await stable("lazy", api.probeSnapshot("lazy"));
check(api.probeSnapshot("lazy").streamNextPolls === 1n, "no prefetch");
check((await lazy.next()).value === 1, "second item");
check((await lazy.next()).done, "Done");
check((await lazy.next()).done, "terminal next");
await lazy.cancel();
await lazy.cancel();
const done = api.probeSnapshot("lazy");
check(done.streamStarts === 1n && done.streamNextPolls === 3n && done.streamDrops === 1n && done.streamTerminalDrops === 1n, "normal drop once");
let replayed = 0;
for await (const _ of lazy) replayed++;
check(replayed === 0, "second traversal after Done is empty");
await stable("lazy", done);

api.resetProbe("idle");
const idle = api.countingStream("idle", 1);
await idle.cancel();
await idle.cancel();
check((await idle.next()).done, "idle closed");
check(api.probeSnapshot("idle").streamStarts === 0n && api.probeSnapshot("idle").streamDrops === 0n, "idle never starts producer");

const optional: Array<number | null> = [];
for await (const value of api.optionalStream("optional")) optional.push(value);
check(optional.length === 3 && optional[0] === 1 && optional[1] === null && optional[2] === 2, "null item is not EOF");

const failing = api.typedErrorStream("typed-error");
check((await failing.next()).value === 7, "item before error");
try {
  await failing.next();
  throw new Error("missing typed error");
} catch (error) {
  if (!(error instanceof api.OutputStreamError)) throw error;
  check(error.variant === "Detailed", "error variant");
  const code: number = error.data.code;
  const message: string = error.data.message;
  check(code === 42 && message === "typed output stream failure", "typed payload");
  // @ts-expect-error no invented payload field
  error.data.retryable;
}
check((await failing.next()).done, "error is terminal");
await failing.cancel();
await failing.cancel();
const failed = api.probeSnapshot("typed-error");
check(failed.streamNextPolls === 2n && failed.streamDrops === 1n && failed.streamTerminalDrops === 1n, "error drops once");
for await (const _ of failing) replayed++;
check(replayed === 0, "second traversal after Error is empty");
await stable("typed-error", failed);

for (const method of ["cancel", "return"] as const) {
  const id = `pending-${method}`;
  const pending = api.pendingStream(id);
  // Attach rejection handling before cancellation; no unhandled rejection window.
  const read = pending.next().then(
    () => { throw new Error("pending next unexpectedly succeeded"); },
    error => { check(error instanceof api.UniffiError && error.errorName === "UniffiCancelled", "pending cancellation"); },
  );
  await untilPolled(id);
  try {
    await pending.next();
    throw new Error("concurrent next accepted");
  } catch (error) {
    check(error instanceof api.UniffiError && error.errorName === "UniffiStreamConcurrentNext", "concurrent next rejected");
  }
  if (method === "return") await pending[Symbol.asyncIterator]().return?.();
  else await pending.cancel();
  await read;
  await pending.cancel();
  check((await pending.next()).done, "cancelled next remains Done");
  const closed = api.probeSnapshot(id);
  check(closed.streamStarts === 1n && closed.streamNextPolls === 1n && closed.streamDrops === 1n && closed.streamCancelledDrops === 1n, "pending cleanup acknowledged");
  await stable(id, closed);
}

const owner = api.StreamOwner.new("object-break");
const owned: UniFfiStream<number> = owner.values(3);
for await (const value of owned) {
  check(value === 0, "object first item");
  owner.dispose();
  break; // for-await calls and awaits return().
}
const broken = api.probeSnapshot("object-break");
check(broken.objectDrops === 1n && broken.streamDrops === 1n && broken.streamCancelledDrops === 1n && broken.streamNextPolls === 1n, "break and object release");
await owned.cancel();
await stable("object-break", broken);

const asyncOwner = api.StreamOwner.new("async-object");
const asyncStream: UniFfiStream<number> = await asyncOwner.pending();
check(api.probeSnapshot("async-object").streamNextPolls === 0n, "async factory does not pull");
await asyncStream.cancel();
await asyncStream.cancel();
asyncOwner.dispose();
check(api.probeSnapshot("async-object").objectDrops === 1n, "async owner released");
console.log("OUTPUT_STREAMS_WASM_OK");
