# Async/Future support

UniFFI supports exposing async Rust functions over the FFI. It can convert a Rust `Future`/`async fn` to and from foreign native futures (`async`/`await` in Python/Swift/Ruby, `suspend fun` in Kotlin etc.)

Check out the [examples](https://github.com/mozilla/uniffi-rs/tree/main/examples/futures) or the more terse and thorough [fixtures](https://github.com/mozilla/uniffi-rs/tree/main/fixtures/futures).

We've also [documentation on the internals of how this works](internals/async-overview.md).

## Shared Rust async runtime

This fork enables `uniffi/async-runtime` by default. It provides task spawning,
local executors, cancellation contexts, timers and demand-driven streams through
`uniffi::async_runtime`. A build with `default-features = false` must explicitly
enable `async-runtime` when exporting local objects or the complete stream API.
The lower-level `uniffi_core` crate can still be built without the runtime.

Free functions and inherent methods keep their original Rust signatures. The
exported result is obtained from one `async fn` or one explicitly returned Future:

```rust
use uniffi::async_runtime::{BoxFuture, BoxStream};

#[uniffi::export]
pub fn next_value(value: u32) -> BoxFuture<'static, u32> {
    Box::pin(async move { value + 1 })
}

#[uniffi::export]
pub fn optional_values() -> BoxStream<'static, Option<u32>> {
    Box::pin(uniffi::deps::futures_util::stream::iter([None, Some(7)]))
}
```

Supported declarations include `impl Future<Output = T>`,
`Pin<Box<dyn Future<Output = T> + ...>>`, `BoxFuture` and `LocalBoxFuture`;
streams accept the corresponding `impl Stream`, pinned boxed stream,
`BoxStream`, `LocalBoxStream` and `UniFfiStream<T, E>` forms. The classifier
recognizes these common names; it does not resolve arbitrary user-defined aliases.
The selected execution boundary must satisfy the actual Rust bounds. Native
shared execution requires `Send`, but never requires the Future to be `Sync`.

`uniffi::async_runtime::BoxFuture` and `BoxStream` include `Send` on native
platforms and omit it on WASM. Use `SendOutsideWasm` and `SyncOutsideWasm` for
portable native-only bounds. WASM tasks and streams remain in their Window or
Dedicated Worker instance; these markers do not transfer a JS value between
realms or create a worker pool.

### Streams and factories

Stream arguments use `UniFfiInputStream<T, E>` for host-produced input on
functions, object methods and callbacks. The `impl` and boxed forms above
describe returned streams; an arbitrary Rust stream argument is not a host
producer handle. Use `std::convert::Infallible` as `E` when an input or
`UniFfiStream` has no business item error.

A stream item can be either `T` or `Result<T, E>`. An `Option<T>` item remains
an item: `None` is distinct from the end of the stream. The first business error
ends the subscription. `Result<Stream, FactoryError>` and an async factory retain
a separate factory error, independently of the stream's item error.
`Result<T, Infallible>` values, factories and stream items keep their original
Rust types while exposing only successful values to the host. An object method
can move its input producer into a returned stream; that subscription then
keeps the producer alive until it ends or closes.

Each subscription has one consumer and allows at most one pending `next()`.
There is no prefetch or unbounded bridge queue. Closing requests cancellation
and waits for stream destruction and registered asynchronous cleanup. An
interrupted pending pull reports framework cancellation; it is not a business
error or a normally produced end marker. In JavaScript, await `stream.return()`
to confirm closure. Creating a stream does not start polling it.

### Local objects

A `LocalExecutor` runs native `!Send` state and futures on their owning thread.
Create the value there and pass its `LocalObject<T>` proxy across threads:

```rust
use std::cell::Cell;
use uniffi::async_runtime::LocalExecutor;

# async fn example() -> Result<(), uniffi::async_runtime::LocalError> {
let executor = LocalExecutor::new(8)?;
let counter = executor.create(|| async { Cell::new(0u32) }).await?;
let value = counter.call(|counter| Box::pin(async move {
    counter.set(counter.get() + 1);
    counter.get()
})).await?;
assert_eq!(value, 1);
counter.close().await?;
executor.close().await?;
# Ok(())
# }
```

For bindings, mark the body with `#[uniffi(local)]` on a derived Object and
its methods with `#[uniffi::export(local)]`. Methods become asynchronous owner
calls, including methods whose Rust body is synchronous. The generated `close`
method waits for cleanup. A local method can construct and return a `!Send`
stream; its body stays on the owner while the subscription proxy travels.
`#[uniffi::export(local)] impl BusinessTrait for Body` exports those methods on
the local Object and preserves the ordinary Rust business trait.

A shared exported trait is a different boundary: its opaque Future/Stream
returns are boxed for dynamic dispatch, preserving `Output`, `Item`, factory
errors and explicit bounds. Use `#[uniffi::export(rust, foreign)]` for host
implementations. Host Future/Stream implementations cannot promise `Sync`;
on WASM they also cannot promise an explicit `Send` bound. These declarations
produce a diagnostic at the bound instead of weakening it. Portable boxed
aliases and `SendOutsideWasm` avoid an unintended WASM `Send` requirement.

Borrowed output values, generic exported data/functions, nested Future returns
and Future/Stream values inside records or containers are rejected at the
unsupported declaration. Await a nested Future in Rust and export its value.
Streams are passed through their dedicated subscription API, not serialized as
data fields.

## Example

This is a short "async sleep()" example:
```Rust
use std::time::Duration;
use async_std::future::{timeout, pending};

/// Async function that says something after a certain time.
#[uniffi::export]
pub async fn say_after(ms: u64, who: String) -> String {
    let never = pending::<()>();
    timeout(Duration::from_millis(ms), never).await.unwrap_err();
    format!("Hello, {who}!")
}
```

This can be called by the following Python code:
```python
import asyncio
from uniffi_example_futures import *

async def main():
    print(await say_after(20, 'Alice'))

if __name__ == '__main__':
    asyncio.run(main())
```

Async functions can also be defined in UDL:
```idl
namespace example {
    [Async]
    string say_after(u64 ms, string who);
}
```

This code uses `asyncio` to drive the future to completion, while our exposed function is used with `await`.

In Rust `Future` terminology this means the foreign bindings supply the "executor" - think event-loop, or async runtime. In this example it's `asyncio`. There's no requirement for a Rust event loop.

This fork also exposes an optional Cargo feature named
`default-async-runtime-tokio`. When enabled on the `uniffi` crate, plain
`#[uniffi::export]` on async functions, methods, and constructors defaults to
Tokio on non-`wasm32` targets when `async_runtime` is omitted. `wasm32`
continues to use the no-runtime path.

There are [some great API docs](https://docs.rs/uniffi_core/latest/uniffi_core/ffi/rustfuture/index.html) on the implementation that are well worth a read.

## Exporting async trait methods

UniFFI is compatible with the [async-trait](https://crates.io/crates/async-trait) crate and this can
be used to export trait interfaces over the FFI.

When using UDL, wrap your trait with the `#[async_trait]` attribute.  In the UDL, annotate all async
methods with `[Async]`:

```idl
[Trait]
interface SayAfterTrait {
    [Async]
    string say_after(u16 ms, string who);
};
```

When using proc-macros, make sure to put `#[uniffi::export]` outside the `#[async_trait]` attribute:

```rust
#[uniffi::export]
#[async_trait::async_trait]
pub trait SayAfterTrait: Send + Sync {
    async fn say_after(&self, ms: u16, who: String) -> String;
}
```

## Combining Rust and foreign async code

Traits with callback interface support that export async methods can be combined with async Rust code.
See the [async-api-client example](https://github.com/mozilla/uniffi-rs/tree/main/examples/async-api-client) for an example of this.

### Python: `uniffi_set_default_event_loop()`

Python bindings export a function named `uniffi_set_default_event_loop()` which handles a corner case when
integrating async Rust and Python code. `uniffi_set_default_event_loop()` is needed when Python async
functions run outside of the eventloop, for example:

* Rust code is executing outside of the eventloop.  Some examples:
    * Rust code spawned its own thread
    * Python scheduled the Rust code using `EventLoop.run_in_executor`
* The Rust code calls a Python async callback method, using something like `pollster` to block
  on the async call.

In this case, we need an event loop to run the Python async function, but there's no eventloop set for the thread.
Use `uniffi_set_default_event_loop()` to handle this case.
It should be called before the Rust code makes the async call and passed an eventloop to use.

Note that `uniffi_set_default_event_loop` cannot be glob-imported because it's not part of the library's `__all__`.

### Ruby: thread-based async

Unlike Python and Swift, Ruby has no native async/await. When Rust calls an async Ruby callback
method, UniFFI spawns a new OS thread for each invocation. There is no event loop requirement,
though a Fiber scheduler is supported for the Ruby -> Rust polling direction.

## Cancelling async code

The shared runtime distinguishes cancellation from a business `Result` error.
`CancellationToken::cancel()` requests cancellation. A `CallContext` waits for
the business Future to be dropped and for cleanup explicitly registered with
`defer_cleanup`; `defer_local_cleanup` retains a `!Send` cleanup future on its
owner. Ordinary Rust `Drop` implementations cannot await work automatically.
Side effects that have already happened are not rolled back.

JavaScript asynchronous calls accept a final `{ signal: AbortSignal }` option.
Their promises settle after Rust and bridge cleanup is confirmed. Cancelling a
subscription stops that subscription, rather than an independently owned
account or background task. A session's `close()` also waits for its active
calls and resources.

Native language cancellation integration is documented with each binding.
A plain Rust task handle detaches on Drop; explicitly cancel and await it when
you own its lifetime. Local object calls and registered foreign calls use the
stronger cancellation-and-cleanup boundary described above.
