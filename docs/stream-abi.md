# Native Stream ABI

This document describes the first implementation slice for native Rust stream returns in UniFFI.

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
non-`'static` streams, methods, and constructors are intentionally rejected in this first slice.

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

`foo_stream_cancel` is idempotent. It removes the stream from the registry, marks the stream closed,
and wakes/drops pending state through the existing future cancellation path.

Concurrent `next` calls for the same stream handle are not supported in this slice. A second `next`
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

The backend `next` result is an object `{ done: boolean, value?: Item }`, not a bare optional item.
This keeps `Stream<Item = Option<T>>` distinguishable from stream completion.

Runtime semantics:

- The iterable can be consumed once. A second `Symbol.asyncIterator()` call throws
  `UniffiStreamConsumed`.
- Overlapping `next()` calls throw `UniffiStreamConcurrentNext`; the Rust stream registry also
  rejects concurrent low-level pulls.
- `break` and manual `return()` call `cancel(handle)` exactly once. Repeated `return()` is
  idempotent.
- A stream item error rejects `next()` through the existing `UniffiError` wrapping path and closes
  the iterator.
- Done closes the iterator. Later `next()` returns `{ done: true }`.

N-API, Electron, and wasm-bindgen backends all emit start / next / cancel exports for stream-returning
free functions. Electron marks only `*_stream_next` as async; start and cancel remain synchronous so
the renderer-side `__call` contract is preserved. The wasm path uses the existing explicit
lowering/lifting helpers and does not introduce `serde` or `serde-wasm-bindgen`.

AbortSignal integration is not part of this phase. Call `return()` or break out of `for await` to
cancel.

## Swift AsyncThrowingStream

The Swift target wraps stream-returning free functions as `AsyncThrowingStream<Item, Error>`:

```swift
for try await item in countEvents(count: 3) {
    // item is the stream item type
}
```

The public Swift API does not expose the raw stream handle. Generated code calls the low-level
start function synchronously to create the handle, then starts a Swift `Task` that repeatedly awaits
the hidden `foo_stream_next(handle)` Rust future through the existing `uniffiRustCallAsync` helper.
The wrapper maps `Ok(Some(item))` to `continuation.yield(item)`, `Ok(None)` to
`continuation.finish()`, and stream item errors to `continuation.finish(throwing:)`.

`continuation.onTermination` cancels the Swift task and calls the hidden `foo_stream_cancel(handle)`.
The Rust-side cancel path is idempotent, so this is safe if Rust has already released the stream after
done or error. A consumer breaking out of `for try await` or otherwise terminating the stream triggers
the same cancel path.

Each call to a stream-returning function owns one Rust stream handle. Treat the returned
`AsyncThrowingStream` as a single-consumer sequence; if multiple independent consumers are needed,
call the Rust function again to create independent handles.

Typed stream error preservation is intentionally deferred. Swift currently exposes the stream as
`AsyncThrowingStream<Item, Error>` and throws the lifted UniFFI error value through the standard Swift
`Error` channel.

## Kotlin Flow

The Kotlin target wraps stream-returning free functions as `kotlinx.coroutines.flow.Flow<Item>`:

```kotlin
countEvents(count = 3u).collect { item ->
    // item is the stream item type
}
```

The public Kotlin API does not expose the raw stream handle. Generated code returns a cold `flow {}`:
each collection synchronously calls the low-level start function to create a new Rust stream handle,
then repeatedly awaits the hidden `foo_stream_next(handle)` Rust future through the existing
`uniffiRustCallAsync` helper. `Some(item)` is emitted, `None` ends the flow, and stream item errors
are thrown to the collector through the standard Kotlin exception path.

The generated flow body always calls hidden `foo_stream_cancel(handle)` from `finally`. This covers
normal completion, collector cancellation, and exceptions. The Rust-side cancel path is idempotent,
so it is safe if Rust has already released the stream after done or error.

Generated Kotlin bindings import `kotlinx.coroutines.flow.Flow` and `kotlinx.coroutines.flow.flow`
when stream-returning functions are present. Consumers must include `kotlinx-coroutines-core` in
their Gradle dependencies, matching the existing UniFFI async support requirement for coroutines.

## Harmony / OpenHarmony

The standalone Harmony JavaScript flavor keeps the standard `common/api.ts` `AsyncIterable<Item>`
API and its explicit pull compatibility helper in `harmony/stream.ts`. The compiled HAR adds a
separate ArkTS-native facade at its package root. The facade is generated from a schema-versioned
stream contract carried through the OHOS host build, checked against the actual N-API type
definitions, and published in the same transaction as the native libraries. The normalized contract
is available both in the dist and at `harmony-facade-contract.json` in the HAR package root. The
generated host crate also owns a checksummed static facade bundle beside its `Cargo.toml`; every
packager invocation reads that bundle directly, so a Cargo-fresh build cannot silently lose APIs.
The package entry explicitly re-exports the raw public types and the Harmony stream interfaces, so
HAR consumers and integrated-HSP Interface HAR consumers import both values and types from the
stable package root rather than from internal native-facade or `.so` declaration paths.

For every stream-returning free function `foo`, the HAR exports both pull and event factories:

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

`start()` is explicit and idempotent. It creates the Rust stream handle only on its first call and
keeps at most one `next` request in flight. Listener dispatch uses a snapshot; one listener throwing
or removing itself does not stop the other listeners or turn into a source error. Normal EOF emits
`done` once. A source error emits one stable-code `BusinessError<UniFfiStreamErrorData<E>>` and then
`done` once. `cancel()` is idempotent, calls the raw cancel function at most once, suppresses any
late in-flight result, and emits `done` once.

The output raw ABI carries the native error name and display detail, but not the original Rust enum
variant. `UniFfiStreamErrorData` therefore exposes `errorType`, `nativeErrorName`, `detail`, and an
unavailable typed `cause`; it does not guess a Rust variant from an N-API error name. Library-owned
error categories are stable and distinct:

- `1900001`: output source failure;
- `1900002`: client misuse, currently concurrent pull `next()`;
- `1900003`: caller-supplied typed input failure;
- `1900004`: write attempted after input termination.

The raw start/next/cancel exports remain public. The packaged pull interface is:

```ts
export interface UniFfiStream<T> {
    next(): Promise<UniFfiStreamResult<T>>;
    cancel(): Promise<void>;
}
```

The event and pull objects are confined to the ArkTS concurrency instance that creates them. They do
not implement `Sendable` and must not be transferred to a Worker or TaskPool.

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

The Harmony HAR additionally exports one named push channel per distinct item/error pair. The
generated factory name contains a readable item/error prefix plus a stable canonical fingerprint;
the example below aliases that generated factory as `createEventsInputChannel`:

```ts
const channel = createEventsInputChannel();
const result = sumEvents(channel.source);
await channel.writer.write({ value: 1 });
channel.writer.end();
await result;
```

`write()` resolves only after a native `next` callback takes that item. This one-item acknowledgement
is the backpressure contract; awaiting `write()` before starting the native consumer intentionally
waits. `end()`, typed `fail(new UniFfiInputFailure(...))`, and native cancellation settle any waiting
`next`, reject unconsumed or subsequent writes with a stable BusinessError-compatible error, and
never leave a writer promise pending. `fail()` resolves the native callback with a typed error
envelope rather than rejecting its promise, so the existing UniFFI error lowerer receives the exact
Rust error value.

A source is one logical FIFO stream and may have multiple waiting native consumers. Each item is
delivered to the oldest waiter. `end()` and native cancellation resolve every waiter with EOF;
`fail()` broadcasts the typed error envelope to all waiters already present and later pulls observe
EOF. A native cancellation from any consumer closes the shared logical source for every consumer.
The callback handle is a non-zero object-local token used to reject mismatched callback invocations;
it is not a process-global identity and may be reused after its 32-bit counter wraps.

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
`AsyncIterable<T>`, Swift as `AsyncThrowingStream<T, Error>`, and Kotlin as `Flow<T>`.

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
