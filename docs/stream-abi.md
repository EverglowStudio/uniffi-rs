# Native Stream ABI

This document describes the current native Rust output- and input-stream ABI in UniFFI.

## Supported Rust Shape

The proc-macro path recognizes top-level functions returning the portable alias:

```rust
#[uniffi::export]
pub fn events() -> uniffi::UniFfiStream<Item, Error> {
    // ...
}
```

`UniFfiStream<T, E>` is the recommended spelling for downstream code because it keeps the
platform-specific `Send` requirement out of application APIs:

- native targets: `Pin<Box<dyn Stream<Item = Result<T, E>> + Send + 'static>>`
- `wasm32` with `wasm-unstable-single-threaded`: `Pin<Box<dyn Stream<Item = Result<T, E>> + 'static>>`

The proc-macro path also recognizes the explicit native shape:

```rust
std::pin::Pin<
    Box<
        dyn futures_core::Stream<Item = Result<Item, Error>> + Send + 'static
    >
>
```

The stream item and error must be UniFFI-supported types. Infallible `Stream<Item = T>`,
non-`'static` streams, methods, and constructors are not part of the current public stream-return
shape.

The explicit `Pin<Box<dyn Stream<...>>>` spelling currently requires `Send + 'static`. Use
`uniffi::UniFfiStream<T, E>` for portable code that must compile both on native targets and on
single-threaded wasm with local, non-`Send` streams such as browser-backed HTTP response streams.

## Low-Level ABI

For a stream-returning function named `foo`, UniFFI exports three low-level functions:

```text
foo(args..., call_status) -> StreamHandle
foo_stream_next(handle) -> RustFuture<Result<Option<Item>, Error>>
foo_stream_cancel(handle) -> void
```

The start function is synchronous. It lowers arguments, calls the Rust function, stores the returned
stream in a Rust-side registry, and returns an opaque stream handle.

`foo_stream_next` reuses the existing Rust future ABI. Bindings poll and complete that future with
the same `rust_future_*` functions used for async Rust functions. The completed value is:

- `Ok(Some(item))` for a yielded stream item.
- `Ok(None)` when the stream is done, cancelled, or the handle is already closed.
- `Err(error)` when the Rust stream yields an expected error.

The future result remains the existing `Result<Option<Item>, Error>` ABI. Its successful RustBuffer
payload uses the normal optional discriminator: outer `0` is Done and outer `1` is Item. If `Item`
is itself optional, the payload has a second tag, so `Stream<Option<u32>>` distinguishes Done
(`0`), Item(None) (`1, 0`), and Item(Some(1)) (`1, 1, 0, 0, 0, 1`). Expected errors remain on the
Rust call-status error channel and are never encoded as Done.

`foo_stream_cancel` is idempotent. It removes the stream from the registry, marks the stream closed,
and wakes/drops pending state through the existing future cancellation path.

Concurrent `next` calls for the same stream handle are not supported. A second `next`
started while another `next` is pending completes through the unexpected-error path with a clear
message. This keeps the ABI single-consumer and pull-based until higher-level bindings add their
native stream wrappers.

## JavaScript AsyncIterable

The JavaScript target wraps the low-level ABI as a single-consumer
`AsyncIterable<Item>`:

```ts
for await (const item of countEvents(3)) {
    // item is the stream item type
}
```

The generated `common/api.ts` starts the stream synchronously, then passes the returned handle to
the shared runtime helper:

```ts
const handle = __call("count_events", count);
return createUniffiAsyncIterable({
    handle,
    next: (handle) => __callAsync("count_events_stream_next", handle),
    cancel: (handle) => __call("count_events_stream_cancel", handle),
});
```

The backend `next` result is an envelope carrying `done`, `value`, and where applicable `error`.
The generated wrapper validates that envelope and converts it to the binding-internal discriminated
result `{ done: false, value: Item } | { done: true }`; it never treats `null` or `undefined` as EOF.
This keeps `Stream<Item = Option<T>>` distinguishable from stream completion. Missing or malformed
envelopes reject with `UniffiStreamProtocolError` instead of silently ending the stream.

Runtime semantics:

- The iterable can be consumed once. A second `Symbol.asyncIterator()` call throws
  `UniffiStreamConsumed`.
- Overlapping `next()` calls throw `UniffiStreamConcurrentNext`; the Rust stream registry also
  rejects concurrent low-level pulls.
- `break` and manual `return()` call `cancel(handle)` exactly once. Repeated `return()` is
  idempotent.
- A stream item error rejects `next()` through the existing `UniffiError` wrapping path, closes the
  iterator, and best-effort cancels the native handle without creating an unhandled cleanup rejection.
- Done closes the iterator. Later `next()` returns `{ done: true }`.
- `throw(error)` best-effort cancels the handle, then rejects with the caller-supplied error.
- The iterable and every iterator obtained from it share one lifetime owner. When an unconsumed
  iterable or abandoned iterator becomes unreachable, a best-effort `FinalizationRegistry` cleanup
  unregisters the owner and cancels the native handle at most once. Finalization timing is not a
  correctness guarantee; callers must still use `return()`, `throw()`, or loop `break` for prompt
  cleanup. Finalizer cleanup swallows synchronous failures and rejected cancellation promises.

N-API, Electron, and wasm-bindgen backends all emit start / next / cancel exports for stream-returning
free functions. Electron marks only `*_stream_next` as async; start and cancel remain synchronous so
the renderer-side `__call` contract is preserved. The wasm path uses the existing explicit
lowering/lifting helpers and does not introduce `serde` or `serde-wasm-bindgen`.

AbortSignal integration is not part of this phase. Call `return()` or break out of `for await` to
cancel.

## Swift UniffiAsyncStream

The Swift target wraps stream-returning free functions as `UniffiAsyncStream<Item>`, a generated
public `AsyncSequence` with a throwing iterator:

```swift
for try await item in countEvents(count: 3) {
    // item is the stream item type
}
```

The public Swift API does not expose the raw stream handle. Generated code calls the low-level start
function synchronously to create that handle, but does not create a native `next` future until a
consumer calls `Iterator.next()`. Each iterator call creates and awaits at most one hidden
`foo_stream_next(handle)` Rust future through `uniffiRustCallAsync`; there is no background producer,
continuation buffering, or prefetch while a consumer is paused.

The wrapper reads the outer RustBuffer optional tag before lifting the item. Outer Item returns an
outer Swift optional containing the item, so `Stream<Option<T>>` returns `.some(nil)` for Item(None).
Only outer Done returns the `nil` which `AsyncIteratorProtocol` uses as EOF. A stream error is thrown
once through Swift's standard `Error` channel, then the iterator is terminal; later `next()` calls
return `nil` without another native pull.

Each generated stream is single-consumer. A second iterator throws an internal consumed-stream error,
and overlapping `next()` calls throw an internal concurrent-next error before reaching Rust. Done is
terminal and later `next()` calls return `nil` without entering native code. Iterator and sequence
release are best-effort cancellation points, so breaking from a `for try await` loop or dropping an
unfinished stream calls the hidden `foo_stream_cancel(handle)` exactly once. Rust's cancel operation
remains idempotent.

Swift task cancellation is propagated to both normal generated async calls and a pending stream next:
the generated helper receives the type-specialized `rust_future_cancel` symbol, calls it from a task
cancellation handler, and serializes native cancel/free under one lifecycle lock. A Rust cancelled
status becomes Swift `CancellationError`; the future free operation still runs exactly once. Cancelling
a pending stream next also best-effort cancels its stream handle, so no late item is delivered after
the task has been cancelled.

`UniffiAsyncStream` deliberately has no public `Sendable` conformance; consume it in the concurrency
context that created it. This is a generated Swift API change from the previous
`AsyncThrowingStream<Item, Error>` return type, so downstream bindings must be regenerated and Swift
call sites which named the old concrete type must migrate. The native stream ABI and exported symbols
are unchanged.

## Kotlin Flow

The Kotlin target wraps stream-returning free functions as `kotlinx.coroutines.flow.Flow<Item>`:

```kotlin
countEvents(count = 3u).collect { item ->
    // item is the stream item type
}
```

The public Kotlin API does not expose the raw stream handle. Generated code returns a cold `flow {}`
whose stream object is single-use: the first collection synchronously calls the low-level start
function to create one Rust stream handle, then repeatedly awaits the hidden
`foo_stream_next(handle)` Rust future through the existing `uniffiRustCallAsync` helper. A
thread-safe collect claim is captured outside `flow {}`; a sequential or concurrent second
collection fails with `InternalException("UniFFI output streams may only be consumed once")` before
argument lowering or native start, so it cannot create another handle. The first collection remains
lazy until collection begins. The wrapper reads the outer RustBuffer optional tag before it lifts the
item: Item is emitted (including `null` for an optional item), Done ends the flow, and stream item
errors are thrown to the collector through the standard Kotlin exception path.

The generated flow body always calls hidden `foo_stream_cancel(handle)` from `finally`. This covers
normal completion, collector cancellation, and exceptions. The Rust-side cancel path is idempotent,
so it is safe if Rust has already released the stream after done or error.

Generated Kotlin bindings import `kotlinx.coroutines.flow.Flow` and `kotlinx.coroutines.flow.flow`
when stream-returning functions are present. Consumers must include `kotlinx-coroutines-core` in
their Gradle dependencies, matching the existing UniFFI async support requirement for coroutines.

## Harmony / OpenHarmony

The standalone Harmony JavaScript flavor keeps the standard `common/api.ts` `AsyncIterable<Item>`
API and its explicit pull compatibility helper in `harmony/stream.ts`. Packaged HAR and HSP builds
add the ArkTS-native facade at the package root. The facade is generated from a schema-versioned
stream contract carried through the OHOS host build and checked against the actual N-API type
definitions. The normalized contract is available in dist and as
`harmony-facade-contract.json` in the package root. The generated host crate also owns a checksummed
static facade bundle beside its `Cargo.toml`, so a Cargo-fresh packaging build uses the same API
contract.

`Index.ets`, `index.d.ts`, and `Index.d.ets` expose pull and event factories, input-channel
factories, and their public supporting values and types. The raw output start/next/cancel helpers
and each output next-envelope type (including its generated original-name alias) are deliberately
absent from that package-root surface. They remain in `native-facade.ets` and the normalized
`harmony-facade-contract.json` inventory for generated Pull implementation only. Input-stream raw
helpers, ordinary callables, and classes remain package-root exports. Consumers import supported
APIs from the stable package root rather than reaching into the native facade, `src/main/ets`, or
`.so` declaration paths.

For every stream-returning free function `foo`, the package exports both pull and event factories:

```ts
const pull = fooStream(args);
const next = await pull.next();
await pull.cancel();

const events = fooEvents(args);
events.on('data', (item) => { /* consume item */ });
events.on('error', (error) => { /* BusinessError-compatible */ });
events.on('done', () => { /* terminal */ });
events.start();
```

`fooStream(args)` synchronously starts the native output stream and returns the primary
`UniFfiStream<T>` pull facade. Its owner must explicitly call `await cancel()` when it stops before
Done or Error: ArkTS API 20 has no lifecycle finalizer available to provide an abandoned-object
fallback. `fooEvents(args)` is a compatibility adapter over that same Pull facade: it creates the
Pull object only on its first explicit, idempotent `start()`; cancelling before `start()` creates no
native handle. Its pump performs at most one Pull `next()` at a time, and Done, Error, and
`cancel()` all converge through Pull `cancel()`. Listener dispatch uses a snapshot; one listener
throwing or removing itself does not stop the other listeners or turn into a source error. Calling
`cancel()` from a data listener stops later data listeners for that item and prevents another pull.
`off(event)` removes every listener for that event, while `off(event, listener)` removes the selected
listener. Normal EOF emits `done` once. A source error emits one stable-code
`BusinessError<UniFfiStreamErrorData<E>>` and then `done` once. `cancel()` is idempotent, calls the
underlying Pull cancellation at most once, suppresses any late in-flight result, and emits `done`
once. Event delivery actively pumps the Pull stream and therefore does not provide strict consumer
backpressure; use Pull for core or high-throughput data paths and reserve Event for compatibility
and UI notification use cases.

The output raw ABI carries the native error name and display detail, but not the original Rust enum
variant. `UniFfiStreamBusinessError<E>` extends `Error` and implements the Harmony
`BusinessError<UniFfiStreamErrorData<E>>` shape with `code` and `data`.
`UniFfiStreamErrorData` exposes `errorType`, `nativeErrorName`, `detail`, and `cause`; `cause` is
`null` when the raw output ABI cannot carry the typed Rust value. It does not guess a Rust variant
from an N-API error name. Library-owned error categories are stable and distinct:

- `1900001`: output source failure;
- `1900002`: client misuse, currently concurrent pull `next()`;
- `1900003`: caller-supplied typed input failure;
- `1900004`: write attempted after input termination.

The packaged pull interface is:

```ts
export interface UniFfiStream<T> {
    next(): Promise<UniFfiStreamResult<T>>;
    cancel(): Promise<void>;
}
```

The generated pull, event, writer, and input-source objects are confined to the ArkTS concurrency
instance that creates them. They do not implement `Sendable` and must not be transferred to a
Worker or TaskPool.

## Input Streams

UniFFI also supports passing a foreign async stream into Rust by using
`UniFfiInputStream<T, E>` as a direct free-function argument:

```rust
#[uniffi::export]
pub async fn sum_events(
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u64, StreamError> {
    // ...
}
```

The JavaScript target lowers an `AsyncIterable<T>` into an opaque input stream handle. Rust polls
that handle through registered `next` / `cancel` callbacks. Swift maps input streams from
`AsyncSequence`, and Kotlin maps them from `Flow`.

The packaged Harmony root additionally exports one named push channel per distinct item/error pair. The
generated factory name contains a readable item/error prefix plus a stable canonical fingerprint;
the example below aliases that generated factory as `createEventsInputChannel`:

```ts
const channel = createEventsInputChannel();
const result = sumEvents(channel.source);
await channel.writer.write({ value: 1 });
channel.writer.end();
await result;
```

`write()` resolves only after a native `next` callback takes that item. If a native waiter already
exists, delivery and acknowledgement happen immediately; otherwise the writer remains pending. This
per-item acknowledgement is the backpressure contract, so awaiting `write()` before starting the
native consumer intentionally waits. `end()`, typed
`fail(new UniFfiInputFailure(...))`, and native cancellation settle every waiting `next`, reject
unconsumed and subsequent writes with a stable BusinessError-compatible error, and never leave a
writer promise pending. `fail()` resolves current native callbacks with a typed error envelope rather
than rejecting the callback promise, so the existing UniFFI error lowerer receives the exact Rust
error value.

A source is one logical FIFO stream and may have multiple waiting native consumers. Each item is
delivered to the oldest waiter, and queued items preserve writer order. `end()` and native
cancellation resolve every waiter with EOF. `fail()` broadcasts the typed error envelope to all
waiters already present and terminates queued and future writes. If no waiter exists when `fail()`
is called, the channel retains the failure so that the first later valid `next` receives one typed
error envelope; after the failure is delivered, later native pulls observe EOF. A native
cancellation from any consumer closes the shared logical source for every consumer.
The callback handle is a non-zero object-local token used to reject mismatched callback invocations;
it is not a process-global identity and may be reused after its 32-bit counter wraps. A mismatched
`next` invocation observes EOF rather than gaining access to another channel. A mismatched `cancel`
invocation is a no-op and does not close the source; cancellation has no EOF return surface.

The Rust attribute frontend currently exposes input streams only as direct top-level function
arguments. The N-API/Harmony descriptor collector uses one canonical path for every callable kind it
can receive from component metadata, including Rust-owned object, record, and enum
constructors/methods, so raw helpers and channels cannot drift if that metadata is supplied. Input
streams remain unsupported inside records, enums, options, sequences, maps, callback methods, or
error payloads. The input item and error must be UniFFI-supported types. A Harmony stream facade
whose public boundary would require a TypeScript `Record`/index signature (currently a map value)
fails during generation with a focused unsupported-type error instead of publishing invalid ArkTS.

Cancellation is bidirectional:

- If Rust drops the `UniFfiInputStream`, the generated binding calls the foreign iterator's
  `return()` / stream cancellation path.
- If the foreign input iterator throws, Rust sees the configured stream error through the normal
  fallible path.

## Bidirectional Streams

A free function can combine direct input stream arguments with a native stream return:

```rust
#[uniffi::export]
pub fn running_sum(
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    // ...
}
```

This shape is the UniFFI equivalent of a simple bidirectional stream: the foreign side supplies an
input stream, Rust consumes it, and Rust returns an output stream. JavaScript consumes the result as
`AsyncIterable<T>`, Swift as `UniffiAsyncStream<T>`, and Kotlin as `Flow<T>`.

The same single-consumer rules apply on both sides. Breaking out of the output stream cancels the
Rust output stream, which drops the Rust input stream and triggers the foreign input cancellation
path. Errors produced by the input stream propagate through the output stream's error channel when
the Rust stream yields that error.

## Performance and Benchmarks

The stream ABI is pull-based. For output streams, every yielded item requires one foreign-side
`next()` call, one Rust future allocation/poll/completion path, and one item lift. For input streams,
every item requires the symmetric callback into the foreign iterator and one item lower. A
bidirectional stream pays both costs because Rust pulls from the input stream while the foreign side
pulls from the output stream.

This model keeps backpressure and cancellation simple, but it is not a bulk transport. High-frequency
token streams should batch small tokens into larger semantic chunks when possible. For example, an AI
chat API should prefer yielding accumulated text deltas or protocol frames instead of a separate FFI
item for every byte or tiny token. The expected future optimization is a `next_many(max_items)` ABI
that can return multiple ready items per foreign call while preserving the existing single-item
`next()` contract as the portable baseline.

The JavaScript target includes an opt-in benchmark harness for the generated wasm-bindgen and
Node/N-API paths. It covers:

- output stream: Rust `UniFfiStream<T, E>` consumed as JavaScript `AsyncIterable<T>`;
- input stream: JavaScript `AsyncIterable<T>` consumed by Rust `UniFfiInputStream<T, E>`;
- bidirectional stream: JavaScript input stream consumed by Rust while Rust returns an output stream.

Run the benchmark manually:

```sh
cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

The default stream sizes are `100`, `1_000`, and `10_000` items. They can be overridden for quick
local checks:

```sh
UNIFFI_JS_BENCH_ITERS=20 \
UNIFFI_JS_STREAM_BENCH_REPS=1 \
UNIFFI_JS_STREAM_BENCH_COUNTS=100 \
cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

The benchmark prints JSONL rows with `backend`, `case`, `count`, `elapsedMs`, `msPerItem`, and
`itemsPerSec` for stream cases. It gracefully skips when `cargo`, Node with
`--experimental-strip-types`, or the `wasm32-unknown-unknown` target is unavailable.

Default tests do not run the full benchmark. They only run a lightweight smoke test that verifies the
benchmark fixture and driver still contain the output/input/bidirectional stream cases.

Cancellation latency is observable by adding a case that breaks out of `for await` early and records
the time until the input iterator's `return()` or the output stream's cancel path runs. The current
ABI guarantees idempotent cancellation, but in-flight `next()` work may complete or wake before the
cancelled handle is observed.
