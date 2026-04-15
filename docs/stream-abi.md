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

## Next Phase

The next phase wraps this low-level ABI as JavaScript `AsyncIterable`, followed by Swift
`AsyncSequence` / `AsyncThrowingStream` and Kotlin `Flow`.
