# Async/Future support

UniFFI exposes Rust `Future`/`async fn` as Swift async functions, Kotlin suspend
functions, and JavaScript/ArkTS promises. The unified runtime in this fork also
connects streams, cancellation, explicit cleanup, and local objects across these
bindings. Existing Python asyncio and Ruby thread-based async support is described
below; the new unified runtime integration for Python and Ruby is deferred.

Check out the [examples](https://github.com/mozilla/uniffi-rs/tree/main/examples/futures) or the more terse and thorough [fixtures](https://github.com/mozilla/uniffi-rs/tree/main/fixtures/futures).

We've also [documentation on the internals of how this works](internals/async-overview.md).

## Shared Rust async runtime

This fork enables `uniffi/async-runtime` by default. It provides task spawning,
local executors, cancellation contexts, timers and demand-driven streams through
`uniffi::async_runtime`. The `macro-scaffolding` feature also enables this runtime,
so generated calls and stream close acknowledgements work with
`default-features = false`. Backends that generate their own scaffolding must
enable `async-runtime`; the Kotlin JNI runtime already does so.
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
error or a normally produced end marker. In JavaScript, await `stream.cancel()`
to confirm closure, or call `return()` on the iterator obtained from
`stream[Symbol.asyncIterator]()`. Creating a stream does not start polling it.

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

Owner affinity does not serialize whole methods: local calls can interleave at
await points and reenter through another proxy. Keep the executor alive until
all local objects and their streams have closed. Use an actor when a handler
must hold exclusive state across awaits.

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

### Actors

Actors are Rust implementation details behind exported business methods. Keep
`ActorRef` and `ActorOwner` in the service; export methods that await `ask`, return
ordinary records/results, or return streams. Do not serialize actor addresses or
place them in exported records.

```rust
use uniffi::async_runtime::{actor, BoxFuture};
use actor::{Actor, ActorContext, Handler, Message};

struct Counter(u32);
impl Actor for Counter {}
struct Add(u32);
impl Message for Add { type Reply = u32; }
impl Handler<Add> for Counter {
    type Future<'a> = BoxFuture<'a, u32>;
    fn handle<'a>(&'a mut self, message: Add, _context: ActorContext) -> Self::Future<'a> {
        Box::pin(async move { self.0 += message.0; self.0 })
    }
}

# async fn example() -> Result<(), actor::ActorError> {
let (owner, address) = actor::start(Counter(0), 8)?;
address.send(Add(1)).await?; // Bounded admission, not handler completion.
assert_eq!(address.ask(Add(2)).await?, 3);
owner.drain_and_close().await?;
# Ok(())
# }
```

Handlers run one at a time, retaining exclusive access across awaits. `send` (or
`tell`) waits for bounded admission; `try_send` returns `Full(message)` or
`Closed(message)` without waiting. `ask` is lazy and must be awaited; it returns
`Result<Message::Reply, ActorError>` after the request's cleanup. Put business
errors in `Message::Reply`, separately from framework failures.

Request cancellation is cooperative. A running handler must observe its
`ActorContext` token (`is_cancelled`, `cancelled`, or `cancellation_token`) and
restore its invariants before returning. Its `current_context()` is a cleanup
scope, not the request cancellation signal. Do not await another message to the
same actor from inside its handler. For background work, use
`ActorContext::spawn`/`spawn_local` and send a completion message through the
provided `ManagedContext`.

Keep the owner independently of cloned addresses. `owner.close().await` requests
cancellation and waits for handlers, queued-message release, managed work and
cleanup; `drain_and_close().await` stops new admission and drains accepted work.
Dropping the last owner requests close but does not synchronously confirm it.
`actor::start` uses the current native runtime (or current WASM instance).
For native `!Send` state, use `actor::start_local(&executor, factory, capacity)`;
construct that state inside the factory, retain the `LocalExecutor`, await actor
close, and only then call `executor.close().await`. Native Send actors and their
handler futures require `Send`, not `Sync`.

## Calling the same Rust API from each binding

These two exports use the signatures of the unified async fixture. The snippets
assume that this Rust library has been generated for the selected backend and
its module or namespace has been imported. Put the Swift, Kotlin and ArkTS
statements inside an async/throwing or suspend function as appropriate.

```rust
#[uniffi::export]
pub async fn ready(value: u32) -> u32 { value }

#[uniffi::export]
pub fn canonical_infallible() -> uniffi::UniFfiStream<u32, std::convert::Infallible> {
    Box::pin(uniffi::deps::futures_util::stream::iter([Ok(71), Ok(72)]))
}
```

Cancelling a ready call is a race: a value already committed by Rust can still
be returned. Await the call to observe its terminal result and cleanup; a cancel
request alone is not a release acknowledgement.

### Swift

```swift
let call = Task { try await ready(value: 7) }
call.cancel()
do { print(try await call.value) } catch { print(error) }

let values = canonicalInfallible() // UniFfiStream<UInt32>, an AsyncSequence.
do {
    for try await value in values { print(value); break }
} catch {
    try? await values.close()
    throw error
}
try await values.close()
```

Task cancellation reaches Rust. `values.cancel()` only requests cancellation;
`try await values.close()` waits for the pending pull and producer cleanup.
Explicitly close on early exit instead of relying on iterator destruction.

### Kotlin JNA and JNI

```kotlin
// Import the generated functions and kotlinx.coroutines / flow helpers.
// JNI additionally needs: import uniffi.close
coroutineScope {
    val call = launch { println(ready(7u)) }
    call.cancelAndJoin()
}
val values = canonicalInfallible()
try {
    values.take(1).collect { println(it) }
} finally {
    values.close()
}
```

Both backends expose suspend calls and Flow consumption. JNA returns
`UniFfiStream<UInt>`, which implements `Flow` and has a suspend `close` member.
JNI declares `Flow<UInt>` and supplies `uniffi.close` for generated Rust flows;
this extension is not a close operation for arbitrary application flows.
Cancelling the calling Job requests Rust cancellation, and its join waits for
registered cleanup. Flow collection also closes on completion or interruption.

The JNI cancellation bridge uses `kotlinx.coroutines`' internal cancelling-handler
API, with the opt-in contained in the generated runtime. It is validated with
coroutines 1.6.4 and the Android integration's 1.10.2. Dependency upgrades need the
JNI cancellation and cleanup regression checks; internal API stability is not
guaranteed across versions.

### JavaScript / TypeScript

```ts
import { unified_async_fixture as api } from "./generated/node/index.js";

const controller = new AbortController();
const call = api.ready(7, { signal: controller.signal });
controller.abort();
try { console.log(await call); } catch (error) { console.error(error); }

const values = api.canonicalInfallible();
try {
  for await (const value of values) { console.log(value); break; }
} finally {
  await values.cancel();
}
```

Use the selected platform entrypoint; Browser initialization and Electron's
renderer bridge are described in the [JavaScript contract](javascript/contract.md).
`CallOptions` is the final argument of an asynchronous call, not an argument to
`stream.next()`. `for await` early exit calls the iterator's `return()`; explicit
`cancel()` is also awaitable and idempotent. The stream declaration does not
expose a direct `return()` or `throw()` method.

### ArkTS / Harmony

```typescript
import { ArkItemStep, ArkErrorStep, CallController, unified_async_fixture as api } from "./generated/Index";

const controller = new CallController();
const call = api.ready(7, { signal: controller.signal });
controller.abort();
try { console.info(`${await call}`); } catch (error) { console.error(`${error}`); }

const values = api.canonicalInfallible();
try {
  const step = await values.next();
  if (step.kind === "item") console.info(`${(step as ArkItemStep<number>).value}`);
  else if (step.kind === "error") throw (step as ArkErrorStep).error;
} finally {
  await values.cancel();
}
```

Place these statements in an async function. ArkTS uses the generated
`CallController`/`CallSignal` and tagged `item`/`done`/`error` pull results, not a
DOM AbortController or ECMAScript async-iterator syntax. `cancel()` waits for
closure. All four bindings preserve optional items separately from end of stream.

## Existing async example

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

### Python: `uniffi_set_event_loop()`

Python bindings export a function named `uniffi_set_event_loop()` which handles a corner case when
integrating async Rust and Python code. `uniffi_set_event_loop()` is needed when Python async
functions run outside of the eventloop, for example:

* Rust code is executing outside of the eventloop.  Some examples:
    * Rust code spawned its own thread
    * Python scheduled the Rust code using `EventLoop.run_in_executor`
* The Rust code calls a Python async callback method, using something like `pollster` to block
  on the async call.

In this case, we need an event loop to run the Python async function, but there's no eventloop set for the thread.
Use `uniffi_set_event_loop()` to handle this case.
It should be called before the Rust code makes the async call and passed an eventloop to use.

Note that `uniffi_set_event_loop` cannot be glob-imported because it's not part of the library's `__all__`.

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
A result committed before cancellation is preserved, including while its
registered cleanup is still pending. Side effects that have already happened
are not rolled back.

JavaScript asynchronous calls accept a final `{ signal: AbortSignal }` option.
Their promises settle after Rust and bridge cleanup is confirmed. Cancelling a
subscription stops that subscription, rather than an independently owned
account or background task. A session's `close()` also waits for its active
calls and resources.

Native language cancellation integration is documented with each binding.
A plain Rust task handle detaches on Drop; use `TaskHandle::cancel_and_wait()`
when you own its lifetime. Local object calls and registered foreign calls use the
stronger cancellation-and-cleanup boundary described above.
