
private class CloseOnly(private val finish: suspend () -> Unit): UniffiInputStreamProtocol {
    override fun cancel() {}
    override suspend fun close() = finish()
    override fun asFlow(): Flow<*> = emptyFlow<Int>()
    override fun next(callback: UniffiForeignFutureCompleteRustBuffer, callbackData: Long,
                      droppedCallback: UniffiForeignFutureDroppedCallbackStruct) = error("unused next")
}

// Retain an owned payload so coroutine stack-trace recovery cannot replace this
// failure with a message-only copy. All waiters must observe this same result.
private class CloseGateFailure(val payload: Any = Any()): Exception("close failed")

fun main() = runBlocking {
    lazyInputFrameCases()
    terminalAcknowledgementCases()
    withTimeout(10_000) {
        val published = AtomicBoolean(false)
        val completed = AtomicInteger()
        val stream = CloseOnly { check(published.get()) }
        val handle = uniffiInputStreamHandleMap.insert(stream)
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        dropped.onSet = { published.set(true) }
        uniffiInputStreamCloseCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteVoid {
            override fun callback(callbackData: Long, result: UniffiForeignFutureResultVoid.UniffiByValue) {
                check(published.get() && dropped.handle == handle)
                check(result.callStatus.code == 0.toByte())
                assertEquals(0, uniffiInputStreamHandleMap.size)
                completed.incrementAndGet()
                // Rust may drop the waiter synchronously from its completion.
                dropped.callback!!.callback(dropped.handle)
            }
        }, 0, dropped)
        assertEquals(1, completed.get(), "completed close should acknowledge without a dispatch")
        println("PASS close cancellation handle precedes synchronous ACK and reentrant waiter drop")
    }
    withTimeout(10_000) {
        val entered = CompletableDeferred<Unit>()
        val release = CompletableDeferred<Unit>()
        val ended = AtomicBoolean(false)
        val handle = uniffiInputStreamHandleMap.insert(CloseOnly {
            entered.complete(Unit)
            release.await()
            ended.set(true)
        })
        val close = closeInput(handle)
        entered.await()
        assertFalse(close.isCompleted)
        assertEquals(1, uniffiInputStreamHandleMap.size)
        uniffiInputStreamCloseDroppedCallbackImpl.callback(handle)
        assertFalse(close.isCompleted)
        release.complete(Unit)
        assertEquals(0.toByte(), close.await())
        assertTrue(ended.get())
        assertEquals(0, uniffiInputStreamHandleMap.size)
        println("PASS suspended close retains its owner and registration until actual completion")
    }
    withTimeout(10_000) {
        val callbackEntered = CompletableDeferred<Unit>()
        val callbackRelease = java.util.concurrent.CountDownLatch(1)
        val handle = input(flowOf(1))
        uniffiInputStreamNextCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteRustBuffer {
            override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                check(result.callStatus.code == 0.toByte())
                callbackEntered.complete(Unit)
                check(callbackRelease.await(5, java.util.concurrent.TimeUnit.SECONDS))
            }
        }, 0, UniffiForeignFutureDroppedCallbackStruct())
        callbackEntered.await()
        val close = closeInput(handle)
        assertFalse(close.isCompleted, "Job completion alone does not finish its FFI callback")
        callbackRelease.countDown()
        assertEquals(0.toByte(), close.await())
        assertEquals(0, uniffiInputStreamHandleMap.size)
        println("PASS close still joins the complete next callback frame")
    }
    println("3 input close protocol scenarios passed")
}

private suspend fun terminalAcknowledgementCases() = coroutineScope {
    for (business in listOf(false, true)) {
        val expected = IllegalArgumentException("typed error")
        val calls = AtomicInteger()
        val stream = UniFfiStream.rustSubscription<Int>(
            { if (business) __UniffiStreamNext.Error(expected) else __UniffiStreamNext.Done },
            { calls.incrementAndGet() },
        )
        val result = runCatching { stream.toList() }
        if (business) assertSame(expected, result.exceptionOrNull()) else assertEquals(emptyList(), result.getOrThrow())
        stream.close(); stream.close()
        assertEquals(0, calls.get())
        assertFailsWith<InternalException> { stream.toList() }
        println("PASS native typed terminal reuses ACK: business=$business")
    }
    for (failure in listOf("complete", "lift", "free", "cancel")) {
        val expected = IllegalArgumentException(failure)
        val freed = AtomicInteger()
        val closed = AtomicInteger()
        val stream = UniFfiStream.rustSubscription<Int>({
            uniffiRustCallAsync(
                1L, { _, callback, data -> callback.callback(data, 0) }, {},
                { _, status ->
                    if (failure == "complete") throw expected
                    if (failure == "cancel") status.code = 3
                    RustBuffer.ByValue(ByteBuffer.wrap(byteArrayOf(2)))
                },
                { freed.incrementAndGet(); if (failure == "free") throw expected },
                { if (failure == "lift") throw expected
                  __uniffiLiftStreamNext(it, { 1 }, { expected }) }, NullErrorHandler,
            )
        }, { closed.incrementAndGet() })
        val result = runCatching { stream.toList() }
        if (failure == "cancel") assertIs<CancellationException>(result.exceptionOrNull())
        else assertSame(expected, result.exceptionOrNull())
        stream.close()
        assertEquals(1, closed.get()); assertEquals(1, freed.get())
        println("PASS native driver failure retains real close: $failure")
    }
    for (business in listOf(false, true)) for (settled in listOf(false, true)) withTimeout(10_000) {
        val entered = CompletableDeferred<Unit>(); val nextRelease = CompletableDeferred<Unit>()
        val closeEntered = CompletableDeferred<Unit>(); val closeRelease = CompletableDeferred<Unit>()
        val nextReturned = CompletableDeferred<Unit>()
        val calls = AtomicInteger()
        val closeError = CloseGateFailure()
        val businessError = IllegalArgumentException("business")
        val stream = UniFfiStream.rustSubscription<Int>({
            entered.complete(Unit); nextRelease.await()
            nextReturned.complete(Unit)
            if (business) __UniffiStreamNext.Error(businessError) else __UniffiStreamNext.Done
        }, {
            calls.incrementAndGet(); closeEntered.complete(Unit); closeRelease.await(); throw closeError
        })
        val collector = async(start = CoroutineStart.UNDISPATCHED) { runCatching { stream.toList() } }
        entered.await()
        val owner = async(start = CoroutineStart.UNDISPATCHED) { runCatching { stream.close() } }
        closeEntered.await()
        if (settled) {
            closeRelease.complete(Unit)
            assertSame(closeError, owner.await().exceptionOrNull())
        }
        assertFalse(collector.isCompleted)
        val waiter = async(start = CoroutineStart.UNDISPATCHED) { runCatching { stream.close() } }
        nextRelease.complete(Unit); nextReturned.await(); yield()
        if (!settled) {
            assertFalse(owner.isCompleted); assertFalse(waiter.isCompleted); assertFalse(collector.isCompleted)
            closeRelease.complete(Unit)
        }
        assertSame(closeError, owner.await().exceptionOrNull())
        assertSame(closeError, waiter.await().exceptionOrNull())
        val result = collector.await().exceptionOrNull()
        if (business) { assertSame(businessError, result); assertTrue(businessError.suppressed.contains(closeError)) }
        else assertSame(closeError, result)
        assertSame(closeError, runCatching { stream.close() }.exceptionOrNull())
        assertEquals(1, calls.get())
        println("PASS close owner failure cannot be replaced by terminal ACK: business=$business, settled=$settled")
    }
    withTimeout(10_000) {
        val freeEntered = CompletableDeferred<Unit>()
        val freeRelease = java.util.concurrent.CountDownLatch(1)
        val frameReturned = AtomicBoolean(false); val calls = AtomicInteger()
        val stream = UniFfiStream.rustSubscription<Int>({
            try {
                uniffiRustCallAsync(
                    1L, { _, callback, data -> callback.callback(data, 0) }, {},
                    { _, _ -> RustBuffer.ByValue(ByteBuffer.wrap(byteArrayOf(2))) },
                    { freeEntered.complete(Unit); check(freeRelease.await(5, java.util.concurrent.TimeUnit.SECONDS)) },
                    { __uniffiLiftStreamNext(it, { 1 }, { IllegalArgumentException() }) }, NullErrorHandler,
                )
            } finally { frameReturned.set(true) }
        }, { calls.incrementAndGet() })
        val collector = async(Dispatchers.Default) { stream.toList() }
        freeEntered.await()
        try { assertFalse(collector.isCompleted); assertFalse(frameReturned.get()); assertEquals(0, calls.get()) }
        finally { freeRelease.countDown() }
        assertEquals(emptyList(), collector.await()); assertTrue(frameReturned.get())
        stream.close(); assertEquals(0, calls.get())
        println("PASS typed ACK follows actual driver free and next frame return")
    }
    for (pulled in listOf(false, true)) {
        val pulls = AtomicInteger(); val closes = AtomicInteger()
        val stream = UniFfiStream.rustSubscription(
            { pulls.incrementAndGet(); __UniffiStreamNext.Item(1) }, { closes.incrementAndGet() },
        )
        if (pulled) assertEquals(listOf(1), stream.take(1).toList())
        stream.close(); stream.close()
        assertEquals(if (pulled) 1 else 0, pulls.get()); assertEquals(1, closes.get())
        println("PASS native nonterminal/uncollected close preserved: pulled=$pulled")
    }
    println("13 opt3 native terminal ACK scenarios passed")
}

// Inspect private frame state only in this gate; no production observability hook.
private class InputFrameProbe(val stream: UniffiInputStreamProtocol) {
    private val type = stream.javaClass
    private val lock = type.getDeclaredField("lock").also { it.isAccessible = true }.get(stream)
    private val reserve = type.declaredMethods.single { it.name == "reserveLocked" }.also { it.isAccessible = true }
    private val release = type.declaredMethods.single { it.name == "release" }.also { it.isAccessible = true }
    private val pending = type.getDeclaredField("pending").also { it.isAccessible = true }
    private val owner = Any()
    fun reserveFrame(): Any = synchronized(lock) {
        // A callback's pending flag is cleared before its frame exits, allowing reentry.
        pending.setBoolean(stream, false)
        reserve.invoke(stream, owner)
    }
    fun releaseFrame(frame: Any) { release.invoke(stream, frame) }
    fun waiter(frame: Any): Any? = synchronized(lock) {
        frame.javaClass.getDeclaredField("waiter").also { it.isAccessible = true }.get(frame)
    }
    fun lockIsHeld(): Boolean = Thread.holdsLock(lock)
}

private suspend fun lazyInputFrameCases() = coroutineScope {
    fun probe() = InputFrameProbe(UniffiFlowInputStream(emptyFlow<Int>(), { RustBuffer.ByValue() }, { null }))
    run {
        val p = probe(); val frame = p.reserveFrame()
        assertNull(p.waiter(frame))
        p.releaseFrame(frame)
        p.stream.close()
        assertNull(p.waiter(frame), "finished frame must never allocate a close waiter")
        println("PASS K1 frame finished before close needs no Deferred")
    }
    withTimeout(10_000) {
        val p = probe(); val first = p.reserveFrame(); val second = p.reserveFrame()
        assertNull(p.waiter(first)); assertNull(p.waiter(second))
        val closer = launch(start = CoroutineStart.UNDISPATCHED) { p.stream.close() }
        val firstWaiter = p.waiter(first)
        check(firstWaiter is Job)
        assertNull(p.waiter(second))
        val other = launch(start = CoroutineStart.UNDISPATCHED) { p.stream.close() }
        assertSame(firstWaiter, p.waiter(first))
        val resumed = AtomicBoolean(false)
        firstWaiter.invokeOnCompletion { check(!p.lockIsHeld()); resumed.set(true) }
        closer.cancel()
        p.releaseFrame(second)
        assertNull(p.waiter(second), "snapshot frame finished before its await needs no waiter")
        assertFalse(closer.isCompleted); assertFalse(other.isCompleted)
        p.releaseFrame(first)
        joinAll(closer, other)
        assertTrue(resumed.get())
        println("PASS K1 lazy waiter reuse, snapshot release race, cancelled closer and lock-free wake")
    }
    withTimeout(10_000) {
        val secondEntered = CompletableDeferred<Unit>()
        val releaseFirst = java.util.concurrent.CountDownLatch(1)
        val releaseSecond = java.util.concurrent.CountDownLatch(1)
        val handle = input(flowOf(1, 2))
        val second = object: UniffiForeignFutureCompleteRustBuffer {
            override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                assertEquals(0.toByte(), result.callStatus.code)
                secondEntered.complete(Unit)
                check(releaseSecond.await(5, java.util.concurrent.TimeUnit.SECONDS))
            }
        }
        uniffiInputStreamNextCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteRustBuffer {
            override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                assertEquals(0.toByte(), result.callStatus.code)
                uniffiInputStreamNextCallbackImpl.callback(handle, second, 0, UniffiForeignFutureDroppedCallbackStruct())
                check(releaseFirst.await(5, java.util.concurrent.TimeUnit.SECONDS))
            }
        }, 0, UniffiForeignFutureDroppedCallbackStruct())
        secondEntered.await()
        val close = closeInput(handle)
        assertFalse(close.isCompleted)
        releaseSecond.countDown()
        yield()
        assertFalse(close.isCompleted, "first callback frame still owns completion after reentrant next")
        releaseFirst.countDown()
        assertEquals(0.toByte(), close.await())
        assertEquals(0, uniffiInputStreamHandleMap.size)
        println("PASS K1 reentrant next keeps both callback frames in close ownership")
    }
    withTimeout(10_000) {
        val ack = CompletableDeferred<Byte>()
        val stream = UniffiFlowInputStream(flowOf(1), { _: UniffiInputStreamNext<Int> -> throw IllegalArgumentException("lower failed") }, { null })
        stream.next(object: UniffiForeignFutureCompleteRustBuffer {
            override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                check(ack.complete(result.callStatus.code))
            }
        }, 0, UniffiForeignFutureDroppedCallbackStruct())
        assertEquals(UNIFFI_CALL_UNEXPECTED_ERROR, ack.await())
        stream.close()
        assertEquals(0, uniffiInputStreamNextJobHandleMap.size)
        println("PASS K1 lower error retains unique error ACK and releases its frame")
    }
    withTimeout(10_000) {
        val expected = CloseGateFailure()
        val reported = CompletableDeferred<Throwable>()
        val calls = AtomicInteger()
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { _, error -> reported.complete(error) }
        try {
            val handle = input(flowOf(1))
            uniffiInputStreamNextCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteRustBuffer {
                override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                    assertEquals(0.toByte(), result.callStatus.code)
                    calls.incrementAndGet()
                    throw expected
                }
            }, 0, UniffiForeignFutureDroppedCallbackStruct())
            val failure = reported.await()
            assertTrue(generateSequence(failure) { it.cause }.any { it === expected })
            assertEquals(0.toByte(), closeInput(handle).await())
            assertEquals(1, calls.get())
            assertEquals(0, uniffiInputStreamNextJobHandleMap.size)
            assertEquals(0, uniffiInputStreamHandleMap.size)
        } finally { Thread.setDefaultUncaughtExceptionHandler(previous) }
        println("PASS K1 thrown FFI callback releases its frame once and does not strand close")
    }
    println("5 K1 lazy frame protocol scenarios passed")
}
