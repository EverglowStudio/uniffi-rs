# Kotlin Lifetimes

Ordinary Kotlin object wrappers register a Cleaner for their Rust reference.
Temporary service wrappers therefore do not require a chain of manual releases.
For deterministic release of an ordinary wrapper, `destroy()` remains available;
objects without a business `close` method also implement `AutoCloseable`.

## Asynchronous resources

Declare a business close method once, using source names before Kotlin renames:

```toml
[bindings.lifecycle]
close_methods = { Client = "close" }
```

The method must be asynchronous, take no arguments, and return `()` or
`Result<(), E>`. In JNA and JNI bindings, `client.close()` awaits business cleanup
and releases the wrapper's Rust reference. A subsequent `destroy()` is unnecessary.
The wrapper can remain in an application property without retaining that reference.
Concurrent and repeated closes share the same result, including failure. New
operations fail once close starts; operations already admitted to Rust own their
references and remain safe.

The close runs in `NonCancellable`, so cancellation of a waiting coroutine cannot
interrupt cleanup. This suspending method does not implement synchronous
`AutoCloseable`. A generated member `use` supplies a suspending resource scope:

```kotlin
client.use { resource ->
    resource.load()
} // waits for close on success, error, or cancellation
```

When both the block and close fail, the block's error is rethrown and the close
error is attached as a suppressed exception. The low-level `Disposable` extension
`use` remains a reference-release helper for ordinary wrappers; a resource's
member `use` performs its business close as well.

## Cancellation and streams

Ordinary suspending calls retain their existing cancellation behavior. Use
`cancelAndJoin()` (or cancel then join) to confirm Rust and callback cleanup.
Output streams implement `Flow`. Collector completion, failure, `take`, and
coroutine cancellation execute an awaited, noncancellable close in `finally`.
An explicit stream `close()` also waits for cleanup. Each subscription and resource
has independent close state; ending one does not close another client or stream.

Lists and maps do not acquire ownership of resource cleanup. Close resources at
their application scope; ordinary contained wrappers continue to use the Cleaner.
