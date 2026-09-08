package uniffi.kotlin_async_contract

import kotlinx.coroutines.*
import kotlin.test.*
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

// Generated production driver, deterministic FFI functions. These cases cover
// completion/free error ordering on both Ready and suspending cleanup paths.
fun main() = runBlocking {
    for (failureAt in listOf("none", "complete", "lift", "free", "poll")) {
        withTimeout(5_000) {
            val events = mutableListOf<String>()
            val primary = IllegalStateException(failureAt)
            val cleanup = IllegalStateException("free")
            var polls = 0
            val result = runCatching {
                uniffiRustCallAsyncMock(
                    1L,
                    { _, callback, data ->
                        events.add("poll")
                        polls++
                        if (failureAt == "poll" && polls == 1) throw primary
                        callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY)
                    },
                    { events.add("cancel") },
                    { _, _ ->
                        events.add("complete")
                        if (failureAt == "complete") throw primary
                        42
                    },
                    {
                        events.add("free")
                        if (failureAt == "complete" || failureAt == "free") throw cleanup
                    },
                    {
                        events.add("lift")
                        if (failureAt == "lift") throw primary
                        it
                    },
                    UniffiNullRustCallStatusErrorHandler,
                    discardResult = {
                        events.add("discard-start")
                        assertEquals(42, it)
                        assertTrue(currentCoroutineContext().isActive)
                        yield()
                        events.add("discard-end")
                        throw cleanup
                    },
                )
            }
            assertEquals(1, events.count { it == "complete" })
            assertEquals(1, events.count { it == "free" })
            assertTrue(events.indexOf("free") > events.indexOf("complete"))
            if (events.contains("lift")) assertTrue(events.indexOf("free") < events.indexOf("lift"))
            assertEquals(0, uniffiContinuationHandleMap.size)
            when (failureAt) {
                "none" -> {
                    assertEquals(42, result.getOrThrow())
                    assertEquals(listOf("poll", "complete", "free", "lift"), events)
                }
                "free" -> assertSame(cleanup, result.exceptionOrNull())
                else -> {
                    val error = assertNotNull(result.exceptionOrNull())
                    assertEquals<Class<*>>(primary.javaClass, error.javaClass)
                    assertEquals(primary.message, error.message)
                    // Coroutine stack-trace recovery may copy an exception
                    // across the suspending discard, retaining it as cause.
                    assertTrue(error === primary || error.cause === primary)
                    assertEquals(if (failureAt == "lift") emptyList() else listOf(cleanup), primary.suppressed.toList())
                }
            }
            if (failureAt == "poll") {
                assertEquals(
                    listOf("poll", "cancel", "poll", "complete", "free", "lift", "discard-start", "discard-end"),
                    events,
                )
            } else {
                assertFalse(events.contains("cancel"))
                assertFalse(events.contains("discard-start"))
            }
            println("PASS completion/free ordering: $failureAt")
        }
    }
    pollSignalGates()
    consumingTerminalGates()
}

// Only allocation/identity observations use reflection. All result, cancellation,
// exception and drain assertions execute the generated driver and real Deferred.
private fun signalWaiter(signal: UniffiPollSignal): Any? = synchronized(signal) {
    signal.javaClass.getDeclaredField("waiter").apply { isAccessible = true }.get(signal)
}

private suspend fun CoroutineScope.pollSignalGates() {
    var passed = 0
    suspend fun gate(name: String, body: suspend CoroutineScope.() -> Unit) {
        withTimeout(5_000, body)
        assertEquals(0, uniffiContinuationHandleMap.size)
        passed++
        println("PASS lazy poll: $name")
    }

    for (queued in listOf(false, true)) {
        for (failureAt in listOf("none", "complete", "lift", "free")) {
            gate("Ready late cancel queued=$queued failure=$failureAt") {
                val events = mutableListOf<String>()
                val primary = IllegalStateException(failureAt)
                val releaseError = IllegalStateException("release")
                var notification: (() -> Unit)? = null
                var signal: UniffiPollSignal? = null
                var outcome: Result<Int>? = null
                val task = launch(start = CoroutineStart.UNDISPATCHED) {
                    val caller = currentCoroutineContext().job
                    outcome = runCatching {
                        uniffiRustCallAsyncMock(
                            1L,
                            { _, callback, data ->
                                events.add("poll")
                                signal = uniffiContinuationHandleMap.get(data)
                                val notify = { callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY) }
                                if (queued) notification = notify else {
                                    notify()
                                    caller.cancel()
                                }
                            },
                            { events.add("cancel") },
                            { _, _ ->
                                events.add("complete")
                                if (failureAt == "complete") throw primary
                                42
                            },
                            {
                                events.add("free")
                                if (failureAt == "complete" || failureAt == "free") throw releaseError
                            },
                            {
                                events.add("lift")
                                if (failureAt == "lift") throw primary
                                it
                            }, UniffiNullRustCallStatusErrorHandler,
                        )
                    }
                }
                val pending = assertNotNull(signal)
                val waiter = signalWaiter(pending)
                if (queued) {
                    assertIs<CompletableDeferred<*>>(waiter)
                    assertNotNull(notification).invoke()
                    task.cancel() // waiter is queued but has not resumed on this event loop
                } else {
                    assertNull(waiter, "synchronous Ready must not allocate a Deferred")
                }
                task.join()
                assertSame(waiter, signalWaiter(pending))
                val expected = mutableListOf("poll")
                if (queued) expected.add("cancel")
                expected.add("complete")
                expected.add("free")
                if (failureAt != "complete" && failureAt != "free") expected.add("lift")
                assertEquals(expected, events)
                val result = assertNotNull(outcome)
                when (failureAt) {
                    "none" -> assertEquals(42, result.getOrThrow())
                    "free" -> {
                        val error = assertNotNull(result.exceptionOrNull())
                        assertTrue(error === releaseError || error.cause === releaseError)
                    }
                    else -> {
                        val error = assertNotNull(result.exceptionOrNull())
                        assertTrue(error === primary || error.cause === primary)
                        assertEquals(if (failureAt == "lift") emptyList() else listOf(releaseError), primary.suppressed.toList())
                    }
                }
            }
        }
    }

    for (afterWake in listOf(false, true)) {
        gate("prepoll cancellation afterWake=$afterWake") {
            val events = mutableListOf<String>()
            var delivered: Int? = null
            val task = launch(start = CoroutineStart.UNDISPATCHED) {
                val caller = currentCoroutineContext().job
                if (!afterWake) caller.cancel()
                delivered = uniffiRustCallAsyncMock(
                    1L,
                    { _, callback, data ->
                        events.add("poll")
                        val signal = uniffiContinuationHandleMap.get(data)
                        if (afterWake && events.size == 1) {
                            callback.callback(data, 1.toByte())
                            caller.cancel()
                        } else {
                            callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY)
                        }
                        assertNull(signalWaiter(signal))
                    },
                    { events.add("cancel") },
                    { _, _ -> events.add("complete"); 42 },
                    { events.add("free") },
                    { events.add("lift"); it }, UniffiNullRustCallStatusErrorHandler,
                )
            }
            task.join()
            val expected = mutableListOf<String>()
            if (afterWake) expected.add("poll")
            expected.addAll(listOf("cancel", "poll", "complete", "free", "lift"))
            assertEquals(expected, events)
            assertEquals(42, delivered)
        }
    }

    gate("cancel drains delayed original notification using the same waiter") {
        var notify: (() -> Unit)? = null
        var signal: UniffiPollSignal? = null
        val cancelled = CompletableDeferred<Unit>()
        var polls = 0
        var cancels = 0
        var completes = 0
        var frees = 0
        var delivered: Int? = null
        val task = launch(start = CoroutineStart.UNDISPATCHED) {
            delivered = uniffiRustCallAsyncMock(
                1L,
                { _, callback, data ->
                    polls++
                    signal = uniffiContinuationHandleMap.get(data)
                    notify = { callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY) }
                },
                { cancels++; cancelled.complete(Unit) },
                { _, _ -> completes++; 42 }, { frees++ }, { it },
                UniffiNullRustCallStatusErrorHandler,
            )
        }
        val pending = assertNotNull(signal)
        val waiter = assertIs<CompletableDeferred<*>>(signalWaiter(pending))
        try {
            task.cancel()
            cancelled.await()
            yield()
            task.cancel()
            assertSame(waiter, signalWaiter(pending))
            assertFalse(waiter.isCompleted)
            assertFalse(task.isCompleted)
            assertEquals(1, polls)
            assertEquals(1, cancels)
            assertEquals(1, uniffiContinuationHandleMap.size)
            assertEquals(0, completes)
            assertEquals(0, frees)
        } finally {
            assertNotNull(notify).invoke()
            task.join()
        }
        assertEquals(42, delivered)
        assertEquals(1, completes)
        assertEquals(1, frees)
    }

    gate("NC cannot bypass a notified signal's unfinished published waiter") {
        var notify: (() -> Unit)? = null
        var signal: UniffiPollSignal? = null
        var frees = 0
        var completes = 0
        var delivered: Int? = null
        val task = launch(start = CoroutineStart.UNDISPATCHED) {
            delivered = uniffiRustCallAsyncMock(
                1L,
                { _, callback, data ->
                    signal = uniffiContinuationHandleMap.get(data)
                    notify = { callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY) }
                }, {}, { _, _ -> completes++; 42 }, { frees++ }, { it },
                UniffiNullRustCallStatusErrorHandler,
            )
        }
        val pending = assertNotNull(signal)
        // Wrap the real, already awaited Deferred solely to hold its complete
        // call. No replacement state machine or production test hook is used.
        val original = assertIs<CompletableDeferred<*>>(signalWaiter(pending))
        val entered = CountDownLatch(1)
        val release = CountDownLatch(1)
        val draining = CompletableDeferred<Unit>()
        val waits = AtomicInteger()
        val error = AtomicReference<Throwable?>()
        // A forwarding proxy holds complete before invoking the original real
        // Deferred. Every method, including await, uses that same real object.
        val wrapper = java.lang.reflect.Proxy.newProxyInstance(
            CompletableDeferred::class.java.classLoader,
            arrayOf(CompletableDeferred::class.java),
        ) { _, method, args ->
            if (method.name == "await") {
                waits.incrementAndGet()
                draining.complete(Unit)
            }
            if (method.name == "complete") {
                entered.countDown()
                check(release.await(5, TimeUnit.SECONDS))
            }
            try {
                method.invoke(original, *(args ?: emptyArray()))
            } catch (failure: java.lang.reflect.InvocationTargetException) {
                throw failure.targetException
            }
        }
        synchronized(pending) {
            pending.javaClass.getDeclaredField("waiter").apply { isAccessible = true }.set(pending, wrapper)
        }
        val callback = Thread {
            try { assertNotNull(notify).invoke() } catch (failure: Throwable) { error.set(failure) }
        }
        callback.start()
        try {
            assertTrue(entered.await(5, TimeUnit.SECONDS))
            task.cancel()
            draining.await()
            yield()
            assertSame(wrapper, signalWaiter(pending))
            assertEquals(1, waits.get())
            assertFalse(original.isCompleted)
            assertFalse(task.isCompleted)
            assertEquals(0, uniffiContinuationHandleMap.size)
            assertEquals(0, completes)
            assertEquals(0, frees)
        } finally {
            release.countDown()
            callback.join(5_000)
            assertFalse(callback.isAlive)
            task.join()
        }
        assertNull(error.get())
        assertEquals(42, delivered)
        assertEquals(1, completes)
        assertEquals(1, frees)
    }

    gate("duplicate callback still fails at original map removal") {
        val signal = UniffiPollSignal()
        val handle = uniffiContinuationHandleMap.insert(signal)
        uniffiRustFutureContinuationCallbackImpl.callback(handle, UNIFFI_RUST_FUTURE_POLL_READY)
        assertFailsWith<InternalException> {
            uniffiRustFutureContinuationCallbackImpl.callback(handle, 1.toByte())
        }
        assertEquals(UNIFFI_RUST_FUTURE_POLL_READY, signal.await())
        assertNull(signalWaiter(signal))
    }
    println("$passed lazy poll gates passed")
}

private suspend fun CoroutineScope.consumingTerminalGates() {
    for (failureAt in listOf("none", "adapt", "lift", "cancelled")) {
        withTimeout(5_000) {
            val events = mutableListOf<String>()
            val primary = IllegalStateException(failureAt)
            val result = runCatching {
                uniffiRustCallAsync(1L,
                    { _, callback, data -> callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY) },
                    { error("Ready cannot cancel") },
                    { _, status ->
                        events.add("complete")
                        if (failureAt == "cancelled") status.code = 3.toByte()
                        events.add("free")
                        // Models generated external RustBuffer adaptation after
                        // the native consuming function has returned, not a missing symbol.
                        if (failureAt == "adapt") throw primary
                        42
                    }, { error("consumed handle freed twice") },
                    { value ->
                        events.add("lift")
                        assertEquals(listOf("complete", "free", "lift"), events)
                        if (failureAt == "lift") throw primary
                        value
                    }, UniffiNullRustCallStatusErrorHandler)
            }
            when (failureAt) {
                "none" -> assertEquals(42, result.getOrThrow())
                "cancelled" -> assertIs<CancellationException>(result.exceptionOrNull())
                else -> assertSame(primary, result.exceptionOrNull())
            }
            assertEquals(1, events.count { it == "free" })
            assertEquals(if (failureAt == "adapt" || failureAt == "cancelled") 0 else 1, events.count { it == "lift" })
            println("PASS opt8 native ownership survives host conversion: $failureAt")
        }
    }
    withTimeout(5_000) {
        val primary = IllegalStateException("poll")
        val discardError = IllegalStateException("discard")
        val entered = CompletableDeferred<Unit>()
        val release = CompletableDeferred<Unit>()
        val finished = CompletableDeferred<Unit>()
        var polls = 0
        var frees = 0
        var result: Result<Int>? = null
        val task = launch {
            result = runCatching {
                uniffiRustCallAsync(1L,
                    { _, callback, data -> if (++polls == 1) throw primary else callback.callback(data, UNIFFI_RUST_FUTURE_POLL_READY) },
                    {}, { _, _ -> frees++; 42 }, { error("terminal already owns handle") }, { it },
                    UniffiNullRustCallStatusErrorHandler,
                    discardResult = {
                        assertEquals(42, it); assertEquals(1, frees)
                        assertTrue(currentCoroutineContext().isActive)
                        entered.complete(Unit); release.await(); finished.complete(Unit)
                        throw discardError
                    })
            }
        }
        entered.await(); task.cancel()
        assertFalse(task.isCompleted); assertNull(result); assertFalse(finished.isCompleted)
        release.complete(Unit); task.join()
        assertTrue(finished.isCompleted); assertEquals(1, frees)
        val error = assertNotNull(assertNotNull(result).exceptionOrNull())
        assertTrue(error === primary || error.cause === primary)
        assertEquals(listOf(discardError), primary.suppressed.toList())
        println("PASS opt8 consumed factory still waits NC discard and preserves pollingFailure")
    }
    withTimeout(5_000) {
        val primary = IllegalStateException("cancel")
        val cleanup = IllegalStateException("untransferred free")
        var frees = 0
        val result = runCatching {
            uniffiRustCallAsync(1L, { _, _, _ -> error("poll") }, { throw primary },
                { _, _ -> error("terminal must not enter") }, { frees++; throw cleanup },
                { _: Int -> error("lift must not enter") }, UniffiNullRustCallStatusErrorHandler)
        }
        val failure = assertNotNull(result.exceptionOrNull())
        assertTrue(failure === primary || failure.cause === primary)
        assertEquals(listOf(cleanup), failure.suppressed.toList()); assertEquals(1, frees)
        println("PASS opt8 untransferred free preserves original error and suppressed cleanup")
    }
    assertEquals(0, uniffiContinuationHandleMap.size)
    println("6 opt8 consuming terminal scenarios passed")
}
