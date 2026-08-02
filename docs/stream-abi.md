# Native Stream ABI

This document describes the current UniFFI stream contract. It distinguishes a
native **output** stream returned by Rust from a foreign **input** stream passed
to Rust; they are different APIs and must not be conflated.

## Supported Rust output shape

The proc-macro path supports top-level functions returning the portable alias:

```rust
#[uniffi::export]
pub fn events() -> uniffi::UniFfiStream<Item, Error> {
    // ...
}
```

`UniFfiStream<T, E>` is the preferred spelling for downstream code:

- native targets use `Pin<Box<dyn Stream<Item = Result<T, E>> + Send + 'static>>`;
- `wasm32` with `wasm-unstable-single-threaded` uses
  `Pin<Box<dyn Stream<Item = Result<T, E>> + 'static>>`.

The explicit native `Pin<Box<dyn Stream<...>>>` form remains supported on
native targets with `Send + 'static`. Items and errors must be UniFFI-supported
types. Infallible streams, non-`'static` streams, methods, and constructors are
not part of this output-stream shape.

## Strict native output ABI

For a stream-returning free function `foo`, generated scaffolding exports a
start, a pull, and a cancel operation:

```text
foo(args..., call_status) -> StreamHandle
foo_stream_next(handle) -> RustFuture<UniFfiStreamStep<Item, Error>>
foo_stream_cancel(handle) -> void
```

`UniFfiStreamStep` is a strict tagged union:

```text
Item(T) | Done | Error(E)
```

The RustBuffer encoding fixes those tags to byte `1` for `Item`, byte `2` for
`Done`, and byte `3` for `Error`. `Done` has no payload, `Item` carries exactly
one encoded item, and `Error` carries the typed declared error. Unknown tags,
extra `Done` payloads, and malformed steps are rejected. A concurrent
low-level pull is a misuse error; it is not a second consumer.

Completion is never represented by a nullable item. In particular,
`UniFfiStream<Option<T>, E>` distinguishes `Item(None)` from `Done`. There is
no nullable-EOF decoder, old-symbol fallback, or legacy step alias.

This is a breaking native output ABI change. Regenerate every binding together
with the Rust library; mixed old and new generated bindings are unsupported.

## Output-stream lifecycle

Generated public output streams are lazy, single-use, and pull-based:

- Constructing the public stream does not call Rust start or allocate a native
  handle. The first consumer pull starts it.
- One public instance has one consumer. A direct pull and an iterator cannot
  both consume it, and overlapping `next()` calls fail rather than issuing a
  second native pull.
- Each public `next()` issues at most one native `*_stream_next` operation.
  There is no background producer, prefetch, or continuation buffer.
- `Done`, `Error`, and cancellation enter one terminal lifecycle. The native
  handle, pending future, and registry entry are released at most once.
- Explicit cancellation is the primary cleanup path. JavaScript finalization is
  only a best-effort fallback: its timing is not a correctness guarantee and
  callers still cancel, return, or break explicitly for prompt cleanup.

Native cancellation is idempotent. Cancelling a pending pull wakes it and makes
that pull observe `Done`; a late item or error cannot revive a terminal public
stream.

## Generated output APIs

### JavaScript

The public output type is:

```ts
export interface UniFfiStream<T> extends AsyncIterable<T> {
  next(): Promise<IteratorResult<T>>;
  cancel(): Promise<void>;
}
```

It supports either controlled direct pulls or one `for await` iterator. Iterator
`return()`/`throw()` and explicit `cancel()` use the same once-only cleanup
path. Rust item errors reject as the generated typed error; malformed native
steps reject as `UniffiStreamProtocolError` rather than silently completing.

Wasm, N-API, and Electron adapters all normalize the same strict tagged step
and typed-error contract. Their internal backend ABI is JavaScript runtime ABI
v2; it is not a public application API.

### Swift

Output functions return `UniFfiStream<Element>`, a lazy, single-use
`AsyncSequence`. `Iterator.next()` starts the stream on its first use and makes
one native pull at a time. A stream item error is thrown, while `Done` is the
only source of the iterator's `nil` EOF.

Cancelling a pending stream `next()` cancels both the Rust future and stream
handle through the Rust cancellation path. Ordinary generated async calls use
the same task-cancellation integration and surface Rust cancellation as
`CancellationError`. The generated stream has no public `Sendable`
conformance.

### Kotlin

Output functions return `UniFfiStream<T> : Flow<T>`, not a bare `Flow<T>`.
The first `collect` starts the native stream; an `AtomicBoolean` claim makes a
sequential or concurrent second collection fail before another start. The
collector's `finally` performs the once-only native cancellation path for
completion, failure, and coroutine cancellation.

This does not change the foreign input-stream API: an input stream argument is
still a producer `Flow<T>` lowered into Rust, not an output `UniFfiStream<T>`.

### Harmony / OpenHarmony

Packaged HAR and HSP consumers import the stable namespace root. Each output
stream is exposed only through its namespace Pull factory/class, for example
`fooStream()` and the associated `next()` / `cancel()` object. The package root
does not expose an Event facade, `fooEvents`, a flat component root, raw output
start/next/cancel functions, or nullable EOF.

The native adapter and public package facade are intentionally separate:

- `native-facade.ets` is the implementation-facing ArkTS adapter;
- `native-facade.d.ts`, staged below `src/main/cpp/types/lib*.so/`, declares
  the raw native module boundary;
- `Index.ets` and the public `Index.d.ets` are the package-root namespace
  surface, with public component `.ets` and `.d.ets` facade modules staged
  under `src/main/ets/components`.

The public declaration uses `.d.ets` so compiled Interface HAR output retains
it. An output error is `UniFfiStreamFailure<E>` and retains its typed
`nativeError: E`; it is not reduced to a debug string or wrapped as a
`BusinessError`. `BusinessError` belongs only to the foreign input-stream
boundary.

## Components, hosts, and managed artifacts

Every component, including a single component, has a stable namespace root.
The generated JavaScript runtime is shared once, and selected N-API, wasm, and
OHOS host builds use a package-level composite host/artifact rather than copied
per-component native binaries. Cross-component types reference their owning
component.

The current exact versions are:

| Boundary | Version |
| --- | --- |
| Harmony facade contract | v4 |
| JavaScript host bundle | v3 |
| JavaScript runtime backend ABI | v2 |
| Managed artifact manifest | v4 |
| HSP facade aggregate contract | v1 |

Managed artifact manifest v4 has canonical ordered `components` and
`hostCompositeIdentity` fields. Generation, reads, and transition validation
require that exact schema; they do not adopt legacy manifests, dual-read old
schemas, or provide compatibility aliases.

HAR is the default package kind. HSP is an explicit package kind, and the two
publish the same public namespace surface. A managed HAR↔HSP transition is
allowed only when the existing generation proves its exact historical routes
and the new invocation proves its current route plan. This is transaction
validation, not a compatibility layer. Core package/HAP validation is separate
from the availability of a standalone CodeLinter executable.

When HSP is selected, `harmony-facade-contract.json` carries the separate,
exact `hspFacadeAggregateSchemaVersion: 1` aggregate. It records the composite
host identity and canonical component and stream inventories; it is not a
legacy-reader bridge for the per-component v4 facade contract.

## wasm-bindgen

The generated wasm host crate and JavaScript glue continue to depend on the
`wasm-bindgen` Rust crate. Build orchestration and tests run
`wasm-bindgen-cli-support` in process. An external `wasm-bindgen` executable,
its CLI, and a source checkout are not prerequisites and are not probed as a
reason to skip a real wasm test.

## Foreign input streams

Input streams reverse ownership: foreign code supplies values to Rust.

- JavaScript input parameters accept `AsyncIterable<T>`.
- Swift input parameters accept `AsyncSequence`.
- Kotlin input parameters accept `Flow<T>`.

These producer contracts remain separate from the output APIs above. Rust polls
the registered foreign `next`/`cancel` callbacks; foreign iterator failures are
lowered through the declared stream error path. A free function may combine an
input stream parameter with an output stream return, but each direction keeps
its own lifecycle and type contract.

## Performance

The output ABI is pull-based: each yielded item requires one foreign-side
`next()` and one native future completion. This gives precise backpressure and
cancellation but is not a bulk transport. High-frequency producers should batch
small values into meaningful chunks when appropriate.

The JavaScript benchmark remains opt-in:

```sh
cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

It measures output, input, and bidirectional stream cases; default tests do
not run the benchmark.
