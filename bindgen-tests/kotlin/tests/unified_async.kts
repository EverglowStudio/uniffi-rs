import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.*
import uniffi.uniffi_bindgen_tests.*

suspend fun eventually(predicate: () -> Boolean) = withTimeout(10_000) {
    while (!predicate()) yield()
}

open class Host: UnifiedHost {
    override suspend fun value(value: UInt): UInt = unifiedReady(value)
    override suspend fun boxed(value: UInt): UInt = unifiedBoxed(value)
    override fun values(count: UInt): Flow<UInt> = flow { repeat(count.toInt()) { emit(it.toUInt()) } }
    override suspend fun produce(count: UInt): Flow<UInt> = values(count)
    override suspend fun consume(source: Flow<UInt>): UInt = source.fold(0u) { sum, value -> sum + value }
    override fun echo(source: Flow<UInt>): Flow<UInt> = source
    override suspend fun echoAsync(source: Flow<UInt>): Flow<UInt> = source
}

var passed = 0
suspend fun nativeCase(name: String, body: suspend CoroutineScope.() -> Unit) {
    withTimeout(15_000, body)
    passed++
    println("PASS JNA $name")
}

runBlocking {
    nativeCase("default runtime success waits cleanup gate and Tokio cleanup timer") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        try {
            val result = async { unifiedRuntimeReady(tracker, cleanup) }
            eventually { tracker.counts().dropped == 1u }
            assertEquals(1u, tracker.counts().started)
            assertEquals(0u, tracker.counts().cleaned)
            assertFalse(result.isCompleted)
            cleanup.open()
            assertEquals(47u, result.await())
            assertEquals(1u, tracker.counts().cleaned)
        } finally { cleanup.open() }
    }
    nativeCase("explicit Tokio pending cancel waits cleanup gate and timer") {
        val tracker = UnifiedTracker()
        val body = UnifiedGate()
        val cleanup = UnifiedGate()
        try {
            val result = launch { unifiedTokioPending(tracker, body, cleanup) }
            eventually { tracker.counts().started == 1u }
            result.cancel()
            eventually { tracker.counts().dropped == 1u }
            assertFalse(result.isCompleted)
            assertEquals(0u, tracker.counts().cleaned)
            result.cancel()
            yield()
            assertFalse(result.isCompleted)
            cleanup.open()
            result.join()
            assertTrue(result.isCancelled)
            assertEquals(1u, tracker.counts().cleaned)
        } finally { body.open(); cleanup.open() }
    }
    nativeCase("Tokio stream first pull timer and close await actual Drop cleanup") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        val stream = unifiedTokioValues(tracker, cleanup)
        val items = mutableListOf<UInt>()
        try {
            assertEquals(0u, tracker.counts().started)
            assertEquals(0u, tracker.counts().polls)
            val collector = launch { stream.take(1).collect { items.add(it) } }
            eventually { tracker.counts().dropped == 1u }
            assertEquals(listOf(47u), items)
            assertEquals(1u, tracker.counts().started)
            assertEquals(0u, tracker.counts().cleaned)
            assertFalse(collector.isCompleted)
            val close = launch(start = CoroutineStart.UNDISPATCHED) { stream.close() }
            close.cancel(); collector.cancel()
            assertFalse(close.isCompleted)
            assertFalse(collector.isCompleted)
            cleanup.open()
            joinAll(close, collector)
            assertEquals(1u, tracker.counts().cleaned)
            stream.close()
            assertEquals(1u, tracker.counts().dropped)
        } finally { cleanup.open(); stream.close() }
    }
    nativeCase("runtime helper follows export impl module placement") {
        val service = UnifiedService()
        try { assertTrue(service.runtimeActive()) }
        finally { service.destroy() }
    }
    nativeCase("ready, explicit Future, boxed Future, Infallible value and unit") {
        assertEquals(4u, unifiedReady(4u))
        assertEquals(5u, unifiedFuture(5u))
        assertEquals(6u, unifiedBoxed(6u))
        assertEquals(7u, unifiedInfallible(7u))
        unifiedInfallibleUnit()
    }
    nativeCase("pending success waits real registered cleanup") {
        val tracker = UnifiedTracker()
        val body = UnifiedGate()
        val cleanup = UnifiedGate()
        try {
            val result = async { unifiedPending(tracker, body, cleanup) }
            eventually { tracker.counts().started == 1u }
            assertFalse(result.isCompleted)
            body.open()
            eventually { tracker.counts().dropped == 1u }
            assertFalse(result.isCompleted)
            assertEquals(0u, tracker.counts().cleaned)
            cleanup.open()
            assertEquals(7u, result.await())
            assertEquals(1u, tracker.counts().cleaned)
        } finally {
            body.open(); cleanup.open()
        }
    }
    nativeCase("Job cancel and second cancel wait Rust Drop and cleanup gate") {
        val tracker = UnifiedTracker()
        val body = UnifiedGate()
        val cleanup = UnifiedGate()
        try {
            val result = launch { unifiedPending(tracker, body, cleanup) }
            eventually { tracker.counts().started == 1u }
            result.cancel()
            eventually { tracker.counts().dropped == 1u }
            assertFalse(result.isCompleted)
            assertEquals(0u, tracker.counts().cleaned)
            result.cancel()
            yield()
            assertFalse(result.isCompleted)
            cleanup.open()
            result.join()
            assertTrue(result.isCancelled)
            assertEquals(1u, tracker.counts().cleaned)
        } finally {
            body.open(); cleanup.open()
        }
    }
    nativeCase("nullable items, EOF and second collect") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        cleanup.open()
        val stream = unifiedValues(3u, false, tracker, cleanup)
        assertEquals(1u, tracker.counts().started)
        assertEquals(0u, tracker.counts().polls)
        assertEquals(listOf(null, 1u, 2u), stream.toList())
        assertEquals(4u, tracker.counts().polls)
        assertEquals(1u, tracker.counts().dropped)
        assertEquals(1u, tracker.counts().cleaned)
        assertFailsWith<InternalException> { stream.toList() }
        stream.close()
        assertEquals(1u, tracker.counts().dropped)
    }
    nativeCase("stream business error delivered once and cleanup awaited") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        val stream = unifiedValues(3u, true, tracker, cleanup)
        val items = mutableListOf<UInt?>()
        try {
            val result = async { runCatching { stream.collect { items.add(it) } } }
            eventually { tracker.counts().dropped == 1u }
            assertEquals(listOf<UInt?>(null), items)
            assertFalse(result.isCompleted)
            assertEquals(2u, tracker.counts().polls)
            cleanup.open()
            assertIs<UnifiedException.Item>(result.await().exceptionOrNull())
            assertEquals(1u, tracker.counts().cleaned)
            assertFailsWith<InternalException> { stream.toList() }
        } finally { cleanup.open() }
    }
    nativeCase("slow collector has no prefetch and concurrent collect fails") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        cleanup.open()
        val first = CompletableDeferred<Unit>()
        val advance = CompletableDeferred<Unit>()
        val stream = unifiedValues(3u, false, tracker, cleanup)
        val collector = launch { stream.collect { if (!first.isCompleted) { first.complete(Unit); advance.await() } } }
        try {
            first.await()
            assertEquals(1u, tracker.counts().polls)
            assertFailsWith<InternalException> { stream.toList() }
            repeat(20) { yield() }
            assertEquals(1u, tracker.counts().polls)
        } finally { advance.complete(Unit) }
        collector.join()
        assertEquals(1u, tracker.counts().cleaned)
    }
    nativeCase("take finally and repeated close share real acknowledgement") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        val stream = unifiedValues(9u, false, tracker, cleanup)
        try {
            val collector = launch { assertEquals(listOf<UInt?>(null), stream.take(1).toList()) }
            eventually { tracker.counts().dropped == 1u }
            val close = launch(start = CoroutineStart.UNDISPATCHED) { stream.close() }
            collector.cancel(); close.cancel()
            assertFalse(collector.isCompleted)
            assertFalse(close.isCompleted)
            assertEquals(1u, tracker.counts().polls)
            cleanup.open()
            joinAll(collector, close)
            assertEquals(1u, tracker.counts().cleaned)
        } finally { cleanup.open() }
    }
    nativeCase("close an uncollected subscription without a pull") {
        val tracker = UnifiedTracker()
        val cleanup = UnifiedGate()
        val stream = unifiedValues(9u, false, tracker, cleanup)
        try {
            val close = launch { stream.close() }
            eventually { tracker.counts().dropped == 1u }
            assertEquals(0u, tracker.counts().polls)
            assertFalse(close.isCompleted)
            cleanup.open()
            close.join()
            assertEquals(1u, tracker.counts().cleaned)
        } finally { cleanup.open() }
    }
    nativeCase("plain and canonical infallible stream declarations") {
        assertEquals(listOf(0u, 1u, 2u), unifiedPlainValues(3u).toList())
        assertEquals(listOf(0u, 1u, 2u), unifiedCanonicalValues(3u).toList())
    }
    nativeCase("async factory error, cancel before subscription, and delivery") {
        val tracker = UnifiedTracker()
        val body = UnifiedGate()
        val cleanup = UnifiedGate()
        cleanup.open()
        val entered = CompletableDeferred<Unit>()
        val cancelled = launch(start = CoroutineStart.UNDISPATCHED) {
            entered.complete(Unit)
            unifiedValuesFactory(2u, false, body, tracker, cleanup)
        }
        entered.await()
        cancelled.cancelAndJoin()
        assertEquals(0u, tracker.counts().started)
        body.open()
        assertFailsWith<UnifiedException.Factory> { unifiedValuesFactory(2u, true, body, tracker, cleanup) }
        val stream = unifiedValuesFactory(2u, false, body, tracker, cleanup)
        assertEquals(1u, tracker.counts().started)
        assertEquals(listOf(null, 1u), stream.toList())
        assertEquals(1u, tracker.counts().cleaned)
    }
    nativeCase("shared object methods and named async constructor") {
        val service = UnifiedService.create()
        try {
            assertEquals(10u, service.future(10u))
            assertEquals(listOf(0u, 1u), service.values(2u).toList())
            assertEquals(listOf(0u, 1u), service.valuesAsync(2u).toList())
            assertEquals(6u, service.sum(flowOf(1u, 2u, 3u)))
        } finally { service.destroy() }
    }
    for (factoryAsync in listOf(false, true)) {
        nativeCase("input transfer to ${if (factoryAsync) "async" else "sync"} output owns producer through close") {
            val service = UnifiedService()
            val produced = AtomicInteger()
            val started = CompletableDeferred<Unit>()
            val finalizing = CompletableDeferred<Unit>()
            val cleanup = CompletableDeferred<Unit>()
            val source = flow {
                try {
                    started.complete(Unit)
                    repeat(99) { produced.incrementAndGet(); emit(it.toUInt()) }
                } finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
            }
            val stream = if (factoryAsync) service.echoAsync(source) else service.echo(source)
            try {
                assertEquals(0, produced.get())
                assertEquals(1, uniffiInputStreamHandleCountUniffiBindgenTests())
                val result = launch { stream.take(1).collect() }
                started.await()
                finalizing.await()
                assertFalse(result.isCompleted)
                assertEquals(1, produced.get())
                assertEquals(1, uniffiInputStreamHandleCountUniffiBindgenTests())
                result.cancel()
                cleanup.complete(Unit)
                result.join()
                assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
            } finally { cleanup.complete(Unit); service.destroy() }
        }
    }
    nativeCase("failed method setup releases unpublished input handle") {
        val service = UnifiedService()
        service.destroy()
        val handles = uniffiInputStreamHandleCountUniffiBindgenTests()
        var starts = 0
        val source = flow { starts++; emit(1u) }
        assertFailsWith<IllegalStateException> { service.echoAsync(source) }
        assertEquals(handles, uniffiInputStreamHandleCountUniffiBindgenTests())
        assertEquals(0, starts)
    }
    for (viaOutput in listOf(false, true)) {
    nativeCase("${if (viaOutput) "pending output next" else "input consumer"} cancel waits Kotlin producer finally") {
        val service = UnifiedService()
        val started = CompletableDeferred<Unit>()
        val finalizing = CompletableDeferred<Unit>()
        val cleanup = CompletableDeferred<Unit>()
        val source = flow<UInt> {
            try { started.complete(Unit); awaitCancellation() }
            finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
        }
        try {
            val result = launch {
                if (viaOutput) service.echo(source).collect() else service.sum(source)
            }
            started.await()
            result.cancel()
            finalizing.await()
            assertFalse(result.isCompleted)
            result.cancel()
            cleanup.complete(Unit)
            result.join()
            assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        } finally { cleanup.complete(Unit); service.destroy() }
    }
    }
    nativeCase("explicit close interrupts pending next and waits producer cleanup") {
        val service = UnifiedService()
        val started = CompletableDeferred<Unit>()
        val finalizing = CompletableDeferred<Unit>()
        val cleanup = CompletableDeferred<Unit>()
        val stream = service.echo(flow {
            try { started.complete(Unit); awaitCancellation() }
            finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
        })
        try {
            val collector = async { runCatching { stream.collect() } }
            started.await()
            val close = launch(start = CoroutineStart.UNDISPATCHED) { stream.close() }
            finalizing.await()
            assertFalse(close.isCompleted)
            assertFalse(collector.isCompleted)
            cleanup.complete(Unit)
            close.join()
            assertIs<CancellationException>(collector.await().exceptionOrNull())
            assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        } finally { cleanup.complete(Unit); service.destroy() }
    }
    nativeCase("host trait value, boxed Future, stream, factory and input reentry") {
        val host = Host()
        assertEquals(12u, unifiedHostValue(host, 12u))
        assertEquals(13u, unifiedHostBoxed(host, 13u))
        assertEquals(6u, unifiedHostValues(host, 4u))
        assertEquals(6u, unifiedHostFactory(host, 4u))
        assertEquals(9u, unifiedHostConsume(host, flowOf(4u, 5u)))
        assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        eventually { uniffiForeignFutureHandleCount() == 0 }
    }
    nativeCase("host factory business error distinct from stream business error") {
        val host = object: Host() {
            override suspend fun produce(count: UInt): Flow<UInt> { throw UnifiedException.Factory() }
            override fun values(count: UInt): Flow<UInt> = flow { emit(1u); throw UnifiedException.Item() }
        }
        assertFailsWith<UnifiedException.Factory> { unifiedHostFactory(host, 3u) }
        assertFailsWith<UnifiedException.Item> { unifiedHostValues(host, 3u) }
        assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
    }
    nativeCase("host callback cancel waits body finally before outer Rust call completes") {
        val started = CompletableDeferred<Unit>()
        val finalizing = CompletableDeferred<Unit>()
        val cleanup = CompletableDeferred<Unit>()
        val host = object: Host() {
            override suspend fun value(value: UInt): UInt {
                try { started.complete(Unit); awaitCancellation() }
                finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
            }
        }
        try {
            val result = launch { unifiedHostValue(host, 8u) }
            started.await()
            result.cancel()
            finalizing.await()
            assertFalse(result.isCompleted)
            result.cancel()
            cleanup.complete(Unit)
            result.join()
            eventually { uniffiForeignFutureHandleCount() == 0 }
        } finally { cleanup.complete(Unit) }
    }
    for (factory in listOf(false, true)) {
        nativeCase("host ${if (factory) "async factory" else "returned stream"} cancellation waits cleanup") {
            val started = CompletableDeferred<Unit>()
            val finalizing = CompletableDeferred<Unit>()
            val cleanup = CompletableDeferred<Unit>()
            val host = object: Host() {
                override fun values(count: UInt): Flow<UInt> = flow {
                    try { started.complete(Unit); awaitCancellation() }
                    finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
                }
                override suspend fun produce(count: UInt): Flow<UInt> {
                    try { started.complete(Unit); awaitCancellation() }
                    finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
                }
            }
            try {
                val result = launch { if (factory) unifiedHostFactory(host, 2u) else unifiedHostValues(host, 2u) }
                started.await()
                result.cancel()
                finalizing.await()
                assertFalse(result.isCompleted)
                result.cancel()
                cleanup.complete(Unit)
                result.join()
                eventually { uniffiForeignFutureHandleCount() == 0 }
                assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
            } finally { cleanup.complete(Unit) }
        }
    }
    nativeCase("Rust implementation exposes every forward trait method") {
        val host = unifiedRustHost()
        try {
            assertEquals(14u, host.value(14u))
            assertEquals(15u, host.boxed(15u))
            assertEquals(listOf(0u, 1u, 2u), host.values(3u).toList())
            assertEquals(listOf(0u, 1u), host.produce(2u).toList())
            assertFailsWith<UnifiedException.Factory> { host.produce(99u) }
            assertEquals(9u, host.consume(flowOf(4u, 5u)))
            assertEquals(listOf(6u, 7u), host.echo(flowOf(6u, 7u)).toList())
            assertEquals(listOf(8u, 9u), host.echoAsync(flowOf(8u, 9u)).toList())
            assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        } finally { (host as Disposable).destroy() }
    }
    for (reverse in listOf(false, true)) {
        for (factoryAsync in listOf(false, true)) {
            nativeCase("${if (reverse) "reverse" else "forward"} trait ${if (factoryAsync) "async" else "sync"} echo keeps input until output cleanup") {
                val host = if (reverse) Host() else unifiedRustHost()
                val produced = AtomicInteger()
                val finalizing = CompletableDeferred<Unit>()
                val cleanup = CompletableDeferred<Unit>()
                val source = flow {
                    try { repeat(99) { produced.incrementAndGet(); emit(it.toUInt()) } }
                    finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
                }
                val output = if (reverse) {
                    if (factoryAsync) unifiedHostEchoAsync(host, source) else unifiedHostEcho(host, source)
                } else {
                    if (factoryAsync) host.echoAsync(source) else host.echo(source)
                }
                val stream = assertIs<UniFfiStream<UInt>>(output)
                try {
                    assertEquals(0, produced.get())
                    val result = launch { assertEquals(listOf(0u), stream.take(1).toList()) }
                    finalizing.await()
                    assertEquals(1, produced.get())
                    assertFalse(result.isCompleted)
                    val close = launch(start = CoroutineStart.UNDISPATCHED) { stream.close() }
                    close.cancel(); result.cancel()
                    assertFalse(close.isCompleted)
                    assertFalse(result.isCompleted)
                    cleanup.complete(Unit)
                    joinAll(result, close)
                    assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
                } finally {
                    cleanup.complete(Unit)
                    stream.close()
                    if (!reverse) (host as Disposable).destroy()
                }
            }
        }
    }
    for (factoryAsync in listOf(false, true)) {
        nativeCase("reverse ${if (factoryAsync) "async" else "sync"} echo closes uncollected input without starting it") {
            var starts = 0
            val source = flow { starts++; emit(1u) }
            val stream = if (factoryAsync) unifiedHostEchoAsync(Host(), source) else unifiedHostEcho(Host(), source)
            assertEquals(0, starts)
            stream.close()
            stream.close()
            assertEquals(0, starts)
            assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        }
        nativeCase("reverse ${if (factoryAsync) "async" else "sync"} echo pending next cancel waits input cleanup") {
            val started = CompletableDeferred<Unit>()
            val finalizing = CompletableDeferred<Unit>()
            val cleanup = CompletableDeferred<Unit>()
            val source = flow<UInt> {
                try { started.complete(Unit); awaitCancellation() }
                finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
            }
            val stream = if (factoryAsync) unifiedHostEchoAsync(Host(), source) else unifiedHostEcho(Host(), source)
            try {
                val result = launch { stream.collect() }
                started.await()
                result.cancel()
                finalizing.await()
                assertFalse(result.isCompleted)
                result.cancel()
                cleanup.complete(Unit)
                result.join()
                assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
            } finally { cleanup.complete(Unit); stream.close() }
        }
    }
    for (cancel in listOf(false, true)) {
        nativeCase("local pending ${if (cancel) "cancel" else "success"} waits owner cleanup gate") {
            val owner = UnifiedOwner()
            val local = UnifiedLocal.create(owner)
            val tracker = UnifiedTracker()
            val body = UnifiedGate()
            val cleanup = UnifiedGate()
            try {
                val result = async { local.pending(tracker, body, cleanup) }
                eventually { tracker.counts().started == 1u }
                if (cancel) result.cancel() else body.open()
                eventually { tracker.counts().dropped == 1u }
                assertEquals(0u, tracker.counts().cleaned)
                assertFalse(result.isCompleted)
                if (cancel) result.cancel()
                cleanup.open()
                if (cancel) {
                    result.join()
                    assertTrue(result.isCancelled)
                } else {
                    assertEquals(0u, result.await())
                }
                assertEquals(1u, tracker.counts().cleaned)
                assertEquals(1u, local.increment())
            } finally {
                body.open(); cleanup.open()
                local.close(); owner.close()
                local.destroy(); owner.destroy()
            }
        }
    }
    nativeCase("owner close cancels local pending method and waits local Drop cleanup") {
        val owner = UnifiedOwner()
        val local = UnifiedLocal.create(owner)
        val tracker = UnifiedTracker()
        val body = UnifiedGate()
        val cleanup = UnifiedGate()
        try {
            val result = async { runCatching { local.pending(tracker, body, cleanup) } }
            eventually { tracker.counts().started == 1u }
            val close = launch { owner.close() }
            eventually { tracker.counts().dropped == 1u }
            assertFalse(close.isCompleted)
            assertFalse(result.isCompleted)
            assertEquals(0u, tracker.counts().cleaned)
            cleanup.open()
            close.join()
            assertTrue(result.await().isFailure)
            assertEquals(1u, tracker.counts().cleaned)
            assertFailsWith<Exception> { local.increment() }
        } finally {
            body.open(); cleanup.open()
            owner.close()
            local.destroy(); owner.destroy()
        }
    }
    for (finish in listOf("success", "cancel", "close", "owner close")) {
        nativeCase("local !Send pending stream $finish waits owner cleanup") {
            val owner = UnifiedOwner()
            val local = UnifiedLocal.create(owner)
            val tracker = UnifiedTracker()
            val body = UnifiedGate()
            val cleanup = UnifiedGate()
            val stream = local.pendingValues(tracker, body, cleanup)
            try {
                assertEquals(1u, tracker.counts().started)
                assertEquals(0u, tracker.counts().polls)
                val result = async { runCatching { stream.toList() } }
                eventually { tracker.counts().polls > 0u }
                val close = when (finish) {
                    "success" -> { body.open(); null }
                    "cancel" -> { result.cancel(); null }
                    "close" -> launch { stream.close() }
                    else -> launch { owner.close() }
                }
                eventually { tracker.counts().dropped == 1u }
                assertFalse(result.isCompleted)
                if (close != null) assertFalse(close.isCompleted)
                assertEquals(0u, tracker.counts().cleaned)
                if (finish == "cancel") result.cancel()
                cleanup.open()
                close?.join()
                if (finish == "cancel") {
                    result.join()
                    assertTrue(result.isCancelled)
                } else if (finish == "success") {
                    assertEquals(listOf(7u), result.await().getOrThrow())
                } else {
                    assertTrue(result.await().isFailure)
                }
                assertEquals(1u, tracker.counts().cleaned)
                stream.close()
                assertEquals(1u, tracker.counts().dropped)
            } finally {
                body.open(); cleanup.open()
                stream.close(); owner.close()
                local.destroy(); owner.destroy()
            }
        }
    }
    nativeCase("local object owner construction, methods, stream poll and close") {
        val owner = UnifiedOwner()
        val local = UnifiedLocal.create(owner)
        try {
            assertEquals(1u, local.increment())
            assertEquals(listOf(0u, 1u, 2u), local.values(3u).toList())
            local.close()
            assertFailsWith<Exception> { local.increment() }
            owner.close()
        } finally { local.destroy(); owner.destroy() }
    }
    assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
    eventually { uniffiForeignFutureHandleCount() == 0 }
    println("$passed real Kotlin JNA unified async cases passed")
}
