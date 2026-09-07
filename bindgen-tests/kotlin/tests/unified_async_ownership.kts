import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlin.test.*
import uniffi.uniffi_bindgen_tests.*

open class OwnershipHost: UnifiedHost {
    override suspend fun value(value: UInt): UInt = value
    override suspend fun boxed(value: UInt): UInt = value
    override fun values(count: UInt): Flow<UInt> = flowOf(count)
    override suspend fun produce(count: UInt): Flow<UInt> = values(count)
    override suspend fun consume(source: Flow<UInt>): UInt = source.fold(0u) { a, b -> a + b }
    override fun echo(source: Flow<UInt>): Flow<UInt> = source
    override suspend fun echoAsync(source: Flow<UInt>): Flow<UInt> = source
}

suspend fun eventuallyOwned(predicate: () -> Boolean) = withTimeout(5_000) {
    while (!predicate()) yield()
}

runBlocking {
    var passed = 0
    for (route in listOf("method", "async method", "reverse", "async reverse")) {
        withTimeout(10_000) {
            val tracker = UnifiedTracker()
            val cleanup = UnifiedGate()
            val service = UnifiedService()
            val source = unifiedTokioValues(tracker, cleanup)
            val output = when (route) {
                "method" -> service.echo(source)
                "async method" -> service.echoAsync(source)
                "reverse" -> unifiedHostEcho(OwnershipHost(), source)
                else -> unifiedHostEchoAsync(OwnershipHost(), source)
            }
            val close = launch(start = CoroutineStart.UNDISPATCHED) { output.close() }
            try {
                eventuallyOwned { tracker.counts().dropped == 1u }
                assertEquals(0u, tracker.counts().started)
                assertEquals(0u, tracker.counts().polls)
                assertEquals(0u, tracker.counts().cleaned)
                assertFalse(close.isCompleted, "outer close must await the uncollected Rust input")
                val repeatedClose = launch(start = CoroutineStart.UNDISPATCHED) { output.close() }
                close.cancel(); repeatedClose.cancel()
                yield()
                assertFalse(close.isCompleted)
                assertFalse(repeatedClose.isCompleted)
                cleanup.open()
                joinAll(close, repeatedClose)
                assertEquals(1u, tracker.counts().cleaned)
                assertEquals(1u, tracker.counts().dropped)
                assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
                println("PASS ownership uncollected Rust input: $route")
                passed++
            } finally {
                cleanup.open()
                output.close(); source.close(); close.join()
                service.destroy(); cleanup.destroy(); tracker.destroy()
            }
        }
    }
    for (viaInput in listOf(false, true)) {
        withTimeout(10_000) {
            val tracker = UnifiedTracker()
            val cleanup = UnifiedGate()
            val source = unifiedTokioValues(tracker, cleanup)
            val entered = CompletableDeferred<Unit>()
            val returned = CompletableDeferred<Unit>()
            val host = object: OwnershipHost() {
                private suspend fun late(result: Flow<UInt>): Flow<UInt> {
                    entered.complete(Unit)
                    try { awaitCancellation() }
                    catch (_: CancellationException) { returned.complete(Unit); return result }
                }
                override suspend fun produce(count: UInt): Flow<UInt> = late(source)
                override suspend fun echoAsync(source: Flow<UInt>): Flow<UInt> = late(source)
            }
            val call = launch(start = CoroutineStart.UNDISPATCHED) {
                if (viaInput) unifiedHostEchoAsync(host, source).close() else unifiedHostFactory(host, 1u)
            }
            try {
                entered.await()
                call.cancel()
                returned.await()
                eventuallyOwned { tracker.counts().dropped == 1u }
                assertFalse(call.isCompleted, "cancel completion must wait for late output disposal")
                assertEquals(0u, tracker.counts().cleaned)
                assertEquals(0u, tracker.counts().polls)
                call.cancel()
                yield()
                assertFalse(call.isCompleted)
                cleanup.open()
                call.join()
                assertTrue(call.isCancelled)
                assertEquals(1u, tracker.counts().cleaned)
                assertEquals(1u, tracker.counts().dropped)
                eventuallyOwned { uniffiForeignFutureHandleCount() == 0 }
                assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
                println("PASS ownership late cancelled foreign output: input adapter=$viaInput")
                passed++
            } finally {
                cleanup.open(); source.close(); call.join()
                cleanup.destroy(); tracker.destroy()
            }
        }
    }
    withTimeout(10_000) {
        val entered = CompletableDeferred<Unit>()
        var starts = 0
        val host = object: OwnershipHost() {
            override suspend fun produce(count: UInt): Flow<UInt> {
                entered.complete(Unit)
                try { awaitCancellation() }
                catch (_: CancellationException) { return flow { starts++; emit(1u) } }
            }
        }
        val call = launch { unifiedHostFactory(host, 1u) }
        entered.await()
        call.cancelAndJoin()
        assertEquals(0, starts, "ordinary cold Flow has no synthetic close/collection")
        eventuallyOwned { uniffiForeignFutureHandleCount() == 0 }
        assertEquals(0, uniffiInputStreamHandleCountUniffiBindgenTests())
        println("PASS ownership cancelled cold Flow result remains unstarted")
        passed++
    }
    println("$passed real Kotlin JNA ownership cases passed")
}
