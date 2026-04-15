# Native Stream ABI

This document describes the first implementation slice for native Rust stream returns in UniFFI.

## Supported Rust Shape

The proc-macro path recognizes top-level functions returning:

```rust
std::pin::Pin<
    Box<
        dyn futures_core::Stream<Item = Result<Item, Error>> + Send + 'static
    >
>
```

The stream item and error must be UniFFI-supported types. Stream parameters, input streams,
bidirectional streams, infallible `Stream<Item = T>`, non-`'static` streams, methods, and constructors
are intentionally rejected in this first slice.

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

## Next Phase

Swift `AsyncSequence` / `AsyncThrowingStream`, Kotlin `Flow`, and Harmony-specific wrappers are
implemented in later phases.
