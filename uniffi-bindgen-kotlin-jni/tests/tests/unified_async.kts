@file:OptIn(kotlin.ExperimentalUnsignedTypes::class)

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlin.test.*
import uniffi.RustStream
import uniffi.StreamStep
import uniffi.close
import uniffi.uniffi_bindgen_tests.*

suspend fun eventually(condition: () -> Boolean) = withTimeout(10_000) {
    while (!condition()) { delay(1) }
}

suspend fun lifecycleContract() = withTimeout(15_000) {
    val tracker = UnifiedTracker()
    val cleanup = UnifiedGate()
    val resource = UnifiedLifecycle(tracker, cleanup, false)
    assertEquals(5u, resource.syncPing())
    assertEquals(3u, resource.plainPing())
    val otherTracker = UnifiedTracker()
    val otherCleanup = UnifiedGate().also { it.open() }
    val other = UnifiedLifecycle(otherTracker, otherCleanup, false)
    try {
        val first = launch { resource.close() }
        eventually { tracker.counts().started == 1u }
        val second = launch(start = CoroutineStart.UNDISPATCHED) { resource.close() }
        first.cancel()
        assertFalse(first.isCompleted)
        assertFalse(second.isCompleted)
        assertFailsWith<IllegalStateException> { resource.ping() }
        assertFailsWith<IllegalStateException> { resource.syncPing() }
        assertEquals(7u, other.ping())
        cleanup.open()
        joinAll(first, second)
        resource.close()
        assertEquals(1u, tracker.counts().started)
        assertEquals(1u, tracker.counts().cleaned)
        assertEquals(1u, tracker.counts().dropped) // wrapper is still strongly held
        assertTrue(resource.uniffiIsDestroyed)
        assertEquals(0u, otherTracker.counts().dropped)
        other.close()
        assertEquals(1u, otherTracker.counts().dropped)
    } finally { cleanup.open(); resource.close(); other.close() }

    val liveTracker = UnifiedTracker()
    val openGate = UnifiedGate().also { it.open() }
    val live = UnifiedLifecycle(liveTracker, openGate, false)
    val body = UnifiedGate()
    try {
        val pending = async { live.pending(body) }
        eventually { liveTracker.counts().polls == 1u }
        live.close()
        assertEquals(0u, liveTracker.counts().dropped) // in-flight FFI clone remains valid
        body.open()
        assertEquals(9u, pending.await())
        assertEquals(1u, liveTracker.counts().dropped)
    } finally { body.open(); live.close() }

    val failedTracker = UnifiedTracker()
    val failed = UnifiedLifecycle(failedTracker, openGate, true)
    val one = async { runCatching { failed.close() }.exceptionOrNull() }
    val two = async { runCatching { failed.close() }.exceptionOrNull() }
    val firstError = assertIs<UnifiedException.Closed>(one.await())
    assertEquals(firstError.message, assertIs<UnifiedException.Closed>(two.await()).message)
    assertEquals(firstError.message, assertIs<UnifiedException.Closed>(runCatching { failed.close() }.exceptionOrNull()).message)
    assertEquals(1u, failedTracker.counts().started)
    assertEquals(1u, failedTracker.counts().dropped)

    val scopeTracker = UnifiedTracker()
    val scoped = UnifiedLifecycle(scopeTracker, openGate, true)
    val bodyError = IllegalArgumentException("scope body failed")
    val observed = runCatching { scoped.use<Unit> { throw bodyError } }.exceptionOrNull()
    assertSame(bodyError, observed)
    assertIs<UnifiedException.Closed>(bodyError.suppressedExceptions.single())
    assertEquals(1u, scopeTracker.counts().dropped)
    val firstTracker = UnifiedTracker()
    val firstCleanup = UnifiedGate()
    val secondTracker = UnifiedTracker()
    val secondCleanup = UnifiedGate().also { it.open() }
    val firstStream = unifiedValues(3u, false, firstTracker, firstCleanup)
    val secondStream = unifiedValues(2u, false, secondTracker, secondCleanup)
    val collecting = launch { firstStream.take(1).collect() }
    try {
        eventually { firstTracker.counts().dropped == 1u }
        collecting.cancel()
        assertFalse(collecting.isCompleted)
        assertEquals(listOf(null, 1u), secondStream.toList())
        assertEquals(1u, secondTracker.counts().cleaned)
        assertEquals(0u, firstTracker.counts().cleaned)
        firstCleanup.open()
        collecting.join()
        assertEquals(1u, firstTracker.counts().cleaned)
    } finally { firstCleanup.open() }
    println("PASS two subscriptions clean up independently")
    println("PASS native resource close: release, concurrent/cancelled waiters, isolation, in-flight calls, failure, use")
}

runBlocking {
    lifecycleContract()
    assertEquals(3u, unifiedReady(3u))
    assertEquals(4u, unifiedFuture(4u))
    assertEquals(5u, unifiedBoxed(5u))
    assertEquals(6u, unifiedInfallible(6u))
    unifiedInfallibleUnit()

    // v3: default runtime must cover Drop and gated asynchronous cleanup,
    // even after the business Future has already returned.
    val runtimeTracker = UnifiedTracker()
    val runtimeCleanup = UnifiedGate()
    val runtimeReady = async { unifiedRuntimeReady(runtimeTracker, runtimeCleanup) }
    eventually { runtimeTracker.counts().dropped == 1u }
    assertEquals(1u, runtimeTracker.counts().started)
    assertEquals(0u, runtimeTracker.counts().cleaned)
    assertFalse(runtimeReady.isCompleted)
    runtimeCleanup.open()
    assertEquals(47u, runtimeReady.await())
    assertEquals(1u, runtimeTracker.counts().cleaned)

    // Explicit Tokio selection on both successful and cancelled calls.
    for (cancel in listOf(false, true)) {
        val tokioTracker = UnifiedTracker()
        val tokioBody = UnifiedGate()
        val tokioCleanup = UnifiedGate()
        val tokioCall = async { unifiedTokioPending(tokioTracker, tokioBody, tokioCleanup) }
        eventually { tokioTracker.counts().started == 1u }
        if (cancel) tokioCall.cancel() else tokioBody.open()
        eventually { tokioTracker.counts().dropped == 1u }
        assertEquals(0u, tokioTracker.counts().cleaned)
        assertFalse(tokioCall.isCompleted)
        if (cancel) tokioCall.cancel()
        tokioCleanup.open()
        if (cancel) {
            assertFailsWith<CancellationException> { tokioCall.await() }
            tokioCall.join()
        } else {
            assertEquals(47u, tokioCall.await())
        }
        assertEquals(1u, tokioTracker.counts().cleaned)
    }

    // Pull/EOF and explicit close each own a Run. Closing before the first
    // pull also needs Tokio during Drop, without starting the stream body.
    for (mode in listOf("eof", "close", "unpolled")) {
        val tokioStreamTracker = UnifiedTracker()
        val tokioStreamCleanup = UnifiedGate()
        val tokioStream = unifiedTokioValues(tokioStreamTracker, tokioStreamCleanup) as RustStream<UInt>
        assertEquals(0u, tokioStreamTracker.counts().polls)
        if (mode != "unpolled") {
            assertEquals(StreamStep.Item(47u), tokioStream.next())
            assertEquals(1u, tokioStreamTracker.counts().started)
        }
        val tokioClose = async {
            if (mode == "eof") assertEquals(StreamStep.Done, tokioStream.next())
            else tokioStream.close()
        }
        eventually { tokioStreamTracker.counts().dropped == 1u }
        assertEquals(0u, tokioStreamTracker.counts().cleaned)
        assertFalse(tokioClose.isCompleted)
        if (mode == "unpolled") {
            assertEquals(0u, tokioStreamTracker.counts().started)
            assertEquals(0u, tokioStreamTracker.counts().polls)
        }
        tokioStreamCleanup.open()
        tokioClose.await()
        tokioStream.close()
        assertEquals(1u, tokioStreamTracker.counts().cleaned)
    }

    val tracker = UnifiedTracker()
    val body = UnifiedGate()
    val cleanup = UnifiedGate()
    val pending = launch { unifiedPending(tracker, body, cleanup) }
    eventually { tracker.counts().started == 1u }
    pending.cancel()
    eventually { tracker.counts().dropped == 1u }
    assertFalse(pending.isCompleted)
    assertEquals(0u, tracker.counts().cleaned)
    pending.cancel()
    cleanup.open()
    pending.join()
    assertEquals(1u, tracker.counts().cleaned)

    val streamTracker = UnifiedTracker()
    val streamCleanup = UnifiedGate()
    val values = unifiedValues(3u, false, streamTracker, streamCleanup) as RustStream<UInt?>
    assertEquals(0u, streamTracker.counts().polls)
    assertEquals(StreamStep.Item(null), values.next())
    assertEquals(1u, streamTracker.counts().polls)
    delay(10)
    assertEquals(1u, streamTracker.counts().polls)
    assertEquals(StreamStep.Item(1u), values.next())
    assertEquals(StreamStep.Item(2u), values.next())
    val eof = async { values.next() }
    eventually { streamTracker.counts().dropped == 1u }
    assertFalse(eof.isCompleted)
    streamCleanup.open()
    assertEquals(StreamStep.Done, eof.await())
    values.close()
    values.close()
    assertEquals(1u, streamTracker.counts().cleaned)

    val concurrentTracker = UnifiedTracker()
    val concurrentGate = UnifiedGate()
    val concurrent = unifiedValues(0u, false, concurrentTracker, concurrentGate) as RustStream<UInt?>
    val accepted = async { concurrent.next() }
    eventually { concurrentTracker.counts().dropped == 1u }
    assertFailsWith<IllegalStateException> { concurrent.next() }
    assertFalse(accepted.isCompleted)
    concurrentGate.open()
    assertEquals(StreamStep.Done, accepted.await())
    concurrent.close()

    val earlyTracker = UnifiedTracker()
    val earlyGate = UnifiedGate()
    val early = unifiedValues(5u, false, earlyTracker, earlyGate)
    val takeOne = async { early.take(1).toList() }
    eventually { earlyTracker.counts().dropped == 1u }
    assertFalse(takeOne.isCompleted)
    assertEquals(1u, earlyTracker.counts().polls)
    earlyGate.open()
    assertEquals(listOf<UInt?>(null), takeOne.await())
    assertEquals(1u, earlyTracker.counts().cleaned)

    val plain = unifiedPlainValues(3u)
    assertEquals(listOf(0u, 1u, 2u), plain.toList())
    assertFailsWith<IllegalStateException> { plain.collect() }
    assertEquals(listOf(0u, 1u), unifiedCanonicalValues(2u).toList())

    val failTracker = UnifiedTracker()
    val failCleanup = UnifiedGate()
    val failing = unifiedValues(3u, true, failTracker, failCleanup) as RustStream<UInt?>
    assertEquals(StreamStep.Item(null), failing.next())
    val failed = async { runCatching { failing.next() } }
    eventually { failTracker.counts().dropped == 1u }
    assertFalse(failed.isCompleted)
    failCleanup.open()
    assertTrue(failed.await().exceptionOrNull() is UnifiedException.Item)
    assertEquals(StreamStep.Done, failing.next())
    failing.close()

    val factoryBody = UnifiedGate()
    val factoryTracker = UnifiedTracker()
    val factoryCleanup = UnifiedGate()
    val factory = async { unifiedValuesFactory(2u, false, factoryBody, factoryTracker, factoryCleanup) }
    assertFalse(factory.isCompleted)
    factoryBody.open()
    val subscription = factory.await()
    assertEquals(0u, factoryTracker.counts().polls)
    val closing = async { subscription.close() }
    eventually { factoryTracker.counts().dropped == 1u }
    assertFalse(closing.isCompleted)
    factoryCleanup.open()
    closing.await()
    assertEquals(1u, factoryTracker.counts().cleaned)
    assertFailsWith<UnifiedException.Factory> { unifiedValuesFactory(1u, true, factoryBody, factoryTracker, factoryCleanup) }

    val service = UnifiedService.create()
    // The export impl lives in runtime_placement, unlike the object type.
    assertTrue(service.runtimeActive())
    assertEquals(8u, service.future(8u))
    assertEquals(listOf(0u, 1u), service.values(2u).toList())
    assertEquals(listOf(0u, 1u), service.valuesAsync(2u).toList())
    assertEquals(6u, service.sum(flowOf(1u, 2u, 3u)))
    for (asyncFactory in listOf(false, true)) {
        var produced = 0
        val sourceEntered = CompletableDeferred<Unit>()
        val sourceClosing = CompletableDeferred<Unit>()
        val sourceCleanup = CompletableDeferred<Unit>()
        val source = flow {
            try {
                produced++
                sourceEntered.complete(Unit)
                emit(9u)
                produced++
                emit(10u)
            } finally {
                sourceClosing.complete(Unit)
                withContext(NonCancellable) { sourceCleanup.await() }
            }
        }
        val echoed = (if (asyncFactory) service.echoAsync(source) else service.echo(source)) as RustStream<UInt>
        assertEquals(0, produced)
        assertEquals(StreamStep.Item(9u), echoed.next())
        sourceEntered.await()
        assertEquals(1, produced)
        val closeOne = async { echoed.close() }
        val closeTwo = async { echoed.close() }
        sourceClosing.await()
        assertFalse(closeOne.isCompleted)
        assertFalse(closeTwo.isCompleted)
        sourceCleanup.complete(Unit)
        closeOne.await()
        closeTwo.await()
        assertEquals(1, produced)
    }

    val host = object : UnifiedHost {
        override suspend fun value(value: UInt): UInt = unifiedReady(value + 1u)
        override suspend fun boxed(value: UInt): UInt = value + 2u
        override fun values(count: UInt): Flow<UInt> = flow { repeat(count.toInt()) { emit(it.toUInt()) } }
        override suspend fun produce(count: UInt): Flow<UInt> = values(count)
        override suspend fun consume(source: Flow<UInt>): UInt = source.toList().fold(0u) { total, v -> total + v }
        override fun echo(source: Flow<UInt>): Flow<UInt> = source
        override suspend fun echoAsync(source: Flow<UInt>): Flow<UInt> = source
    }
    assertEquals(11u, unifiedHostValue(host, 10u))
    assertEquals(12u, unifiedHostBoxed(host, 10u))
    assertEquals(6u, unifiedHostValues(host, 4u))
    assertEquals(6u, unifiedHostFactory(host, 4u))
    assertEquals(6u, unifiedHostConsume(host, flowOf(1u, 2u, 3u)))

    assertEquals(listOf(2u, 3u), unifiedHostEcho(host, flowOf(2u, 3u)).toList())
    assertEquals(listOf(4u, 5u), unifiedHostEchoAsync(host, flowOf(4u, 5u)).toList())
    val rustHost = unifiedRustHost()
    assertEquals(7u, rustHost.value(7u))
    assertEquals(8u, rustHost.boxed(8u))
    assertEquals(listOf(0u, 1u), rustHost.values(2u).toList())
    assertEquals(listOf(0u, 1u), rustHost.produce(2u).toList())
    assertFailsWith<UnifiedException.Factory> { rustHost.produce(99u) }
    assertEquals(3u, rustHost.consume(flowOf(1u, 2u)))
    assertEquals(listOf(6u), rustHost.echo(flowOf(6u)).toList())
    assertEquals(listOf(7u), rustHost.echoAsync(flowOf(7u)).toList())

    // Cancellation races a host factory returning its input. The late output
    // must be disposed and its already-started producer finally acknowledged.
    val lateEntered = CompletableDeferred<Unit>()
    val lateReturn = CompletableDeferred<Unit>()
    val lateFinally = CompletableDeferred<Unit>()
    val lateCleanup = CompletableDeferred<Unit>()
    var lateCleaned = false
    val lateSource = flow {
        try { emit(10u); emit(11u) }
        finally {
            lateFinally.complete(Unit)
            withContext(NonCancellable) { lateCleanup.await() }
            lateCleaned = true
        }
    }
    val lateHost = object : UnifiedHost by host {
        override suspend fun echoAsync(source: Flow<UInt>): Flow<UInt> {
            assertEquals(StreamStep.Item(10u), (source as RustStream<UInt>).next())
            lateEntered.complete(Unit)
            withContext(NonCancellable) { lateReturn.await() }
            return source
        }
    }
    val lateCall = launch { unifiedHostEchoAsync(lateHost, lateSource) }
    lateEntered.await()
    lateCall.cancel()
    lateReturn.complete(Unit)
    lateFinally.await()
    assertFalse(lateCall.isCompleted)
    assertFalse(lateCleaned)
    lateCleanup.complete(Unit)
    lateCall.join()
    assertTrue(lateCleaned)

    val callbackEntered = CompletableDeferred<Unit>()
    val callbackCleanup = CompletableDeferred<Unit>()
    var callbackClosed = false
    val pendingHost = object : UnifiedHost by host {
        override suspend fun value(value: UInt): UInt {
            try { callbackEntered.complete(Unit); awaitCancellation() }
            finally {
                withContext(NonCancellable) { callbackCleanup.await() }
                callbackClosed = true
            }
        }
    }
    val callbackCall = launch { unifiedHostValue(pendingHost, 0u) }
    callbackEntered.await()
    callbackCall.cancel()
    yield()
    assertFalse(callbackCall.isCompleted)
    assertFalse(callbackClosed)
    callbackCall.cancel()
    callbackCleanup.complete(Unit)
    callbackCall.join()
    assertTrue(callbackClosed)

    val owner = UnifiedOwner()
    val local = UnifiedLocal.create(owner)
    assertEquals(1u, local.increment())
    assertEquals(2u, withContext(Dispatchers.Default) { local.increment() })
    assertEquals(listOf(0u, 1u, 2u), local.values(3u).toList())
    val localTracker = UnifiedTracker()
    val localBody = UnifiedGate()
    val localCleanup = UnifiedGate()
    val localCall = launch { local.pending(localTracker, localBody, localCleanup) }
    eventually { localTracker.counts().started == 1u }
    localCall.cancel()
    eventually { localTracker.counts().dropped == 1u }
    assertFalse(localCall.isCompleted)
    assertEquals(0u, localTracker.counts().cleaned)
    localCleanup.open()
    localCall.join()
    assertEquals(1u, localTracker.counts().cleaned)
    val localStreamTracker = UnifiedTracker()
    val localStreamBody = UnifiedGate()
    val localStreamCleanup = UnifiedGate()
    val localStream = local.pendingValues(localStreamTracker, localStreamBody, localStreamCleanup) as RustStream<UInt>
    val localPull = launch { localStream.next() }
    eventually { localStreamTracker.counts().polls > 0u }
    localPull.cancel()
    eventually { localStreamTracker.counts().dropped == 1u }
    assertFalse(localPull.isCompleted)
    assertEquals(0u, localStreamTracker.counts().cleaned)
    localStreamCleanup.open()
    localPull.join()
    assertEquals(1u, localStreamTracker.counts().cleaned)
    localStream.close()
    local.close()
    local.close()
    local.destroy()
    owner.close()
    owner.destroy()
    service.destroy()
    assertTrue(uniffi.JniRuntimeProbe.checkAttachment())
    println("JNI unified async fixture: passed")
}
