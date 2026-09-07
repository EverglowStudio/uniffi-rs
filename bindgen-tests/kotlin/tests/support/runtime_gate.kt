import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.*
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.*

class InternalException(message: String): Exception(message)
class RustBuffer {
    class ByValue(val bytes: ByteBuffer = ByteBuffer.allocate(0)) {
        fun asByteBuffer(): ByteBuffer? = bytes
    }
    companion object {
        fun free(value: ByValue) { value.bytes.position(0) }
    }
}
internal const val UNIFFI_CALL_ERROR: Byte = 1
internal const val UNIFFI_CALL_UNEXPECTED_ERROR: Byte = 2
open class UniffiRustCallStatus(var code: Byte = 0, val error: RustBuffer.ByValue = RustBuffer.ByValue()) {
    class ByValue(code: Byte = 0, error: RustBuffer.ByValue = RustBuffer.ByValue()): UniffiRustCallStatus(code, error)
    companion object {
        fun create(code: Byte, error: RustBuffer.ByValue) = ByValue(code, error)
    }
}
interface UniffiRustCallStatusErrorHandler<E>
object NullErrorHandler: UniffiRustCallStatusErrorHandler<InternalException>
fun <T, E> uniffiRustCallWithError(handler: UniffiRustCallStatusErrorHandler<E>, call: (UniffiRustCallStatus) -> T): T {
    check(handler === NullErrorHandler)
    val status = UniffiRustCallStatus()
    val value = call(status)
    if (status.code == 3.toByte()) throw CancellationException("Rust cancelled")
    check(status.code == 0.toByte())
    return value
}
fun lowerString(value: String) = RustBuffer.ByValue(ByteBuffer.wrap(value.toByteArray()))
interface UniffiRustFutureContinuationCallback { fun callback(data: Long, pollResult: Byte) }
interface UniffiForeignFutureDroppedCallback { fun callback(handle: Long) }
class UniffiForeignFutureDroppedCallbackStruct(var handle: Long = 0, var callback: UniffiForeignFutureDroppedCallback? = null) {
    var onSet: (() -> Unit)? = null
    fun uniffiSetValue(value: UniffiForeignFutureDroppedCallbackStruct) { handle = value.handle; callback = value.callback; onSet?.invoke() }
    fun drop() { callback?.callback(handle) }
}
class UniffiForeignFutureResultRustBuffer {
    class UniffiByValue(val returnValue: RustBuffer.ByValue, val callStatus: UniffiRustCallStatus.ByValue) { fun write() = Unit }
}
class UniffiForeignFutureResultVoid {
    class UniffiByValue(val callStatus: UniffiRustCallStatus.ByValue) { fun write() = Unit }
}
interface UniffiForeignFutureCompleteRustBuffer { fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) }
interface UniffiForeignFutureCompleteVoid { fun callback(callbackData: Long, result: UniffiForeignFutureResultVoid.UniffiByValue) }
interface UniffiInputStreamNextCallback { fun callback(handle: Long, callback: UniffiForeignFutureCompleteRustBuffer, callbackData: Long, droppedCallback: UniffiForeignFutureDroppedCallbackStruct) }
interface UniffiInputStreamCancelCallback { fun callback(handle: Long) }
interface UniffiInputStreamCloseCallback { fun callback(handle: Long, callback: UniffiForeignFutureCompleteVoid, callbackData: Long, droppedCallback: UniffiForeignFutureDroppedCallbackStruct) }
object UniffiLib {
    val CLEANER: java.lang.ref.Cleaner = java.lang.ref.Cleaner.create()
}
// PRODUCTION TEMPLATES

private suspend fun waitUntil(predicate: () -> Boolean) = withTimeout(5_000) {
    while (!predicate()) yield()
}

private class MockFuture {
    val polls = AtomicInteger()
    val cancels = AtomicInteger()
    val frees = AtomicInteger()
    val completes = AtomicInteger()
    private var committed = false
    private var terminalCancelled = false
    private var continuation: Pair<UniffiRustFutureContinuationCallback, Long>? = null
    @Synchronized fun poll(handle: Long, callback: UniffiRustFutureContinuationCallback, data: Long) {
        check(handle == 1L)
        check(continuation == null)
        continuation = callback to data
        polls.incrementAndGet()
    }
    fun wake(ready: Boolean) {
        val waiter = synchronized(this) {
            if (ready) committed = true
            continuation!!.also { continuation = null }
        }
        waiter.first.callback(waiter.second, if (ready) 0 else 1)
    }
    fun cancel(handle: Long) {
        check(handle == 1L)
        cancels.incrementAndGet()
        synchronized(this) { if (!committed) terminalCancelled = true }
        if (synchronized(this) { continuation != null }) wake(false)
    }
    suspend fun call(): Int = uniffiRustCallAsync(
        1L, ::poll, ::cancel,
        { _, status ->
            completes.incrementAndGet()
            if (synchronized(this) { terminalCancelled }) status.code = 3
            42
        },
        { frees.incrementAndGet() }, { it }, NullErrorHandler,
    )
}

private fun input(flow: Flow<Int>): Long = uniffiCreateInputStream(flow, { step ->
    when (step) {
        is UniffiInputStreamNext.Item -> RustBuffer.ByValue(ByteBuffer.allocate(5).put(1).putInt(step.value).flip())
        UniffiInputStreamNext.Done -> RustBuffer.ByValue(ByteBuffer.wrap(byteArrayOf(0)))
    }
}, { error -> if (error is IllegalArgumentException) lowerString(error.message ?: "business") else null })

private fun next(handle: Long, dropped: UniffiForeignFutureDroppedCallbackStruct = UniffiForeignFutureDroppedCallbackStruct()): CompletableDeferred<UniffiForeignFutureResultRustBuffer.UniffiByValue> {
    val completion = CompletableDeferred<UniffiForeignFutureResultRustBuffer.UniffiByValue>()
    uniffiInputStreamNextCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteRustBuffer {
        override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
            check(callbackData == 7L)
            check(completion.complete(result))
        }
    }, 7L, dropped)
    return completion
}

private fun closeInput(handle: Long): CompletableDeferred<Byte> {
    val completion = CompletableDeferred<Byte>()
    uniffiInputStreamCloseCallbackImpl.callback(handle, object: UniffiForeignFutureCompleteVoid {
        override fun callback(callbackData: Long, result: UniffiForeignFutureResultVoid.UniffiByValue) {
            check(callbackData == 8L)
            check(completion.complete(result.callStatus.code))
        }
    }, 8L, UniffiForeignFutureDroppedCallbackStruct())
    return completion
}

fun main() = runBlocking {
    var passed = 0
    suspend fun gate(name: String, block: suspend CoroutineScope.() -> Unit) {
        withTimeout(10_000) { block() }
        println("PASS $name")
        passed++
    }
    gate("future cancel waits for cleanup Ready and frees exactly once") {
        val future = MockFuture()
        val task = launch { future.call() }
        waitUntil { future.polls.get() == 1 }
        task.cancel()
        waitUntil { future.polls.get() == 2 }
        assertFalse(task.isCompleted)
        assertEquals(0, future.frees.get())
        task.cancel()
        future.wake(true)
        task.join()
        assertEquals(1, future.cancels.get())
        assertEquals(1, future.frees.get())
        assertEquals(1, future.completes.get())
        assertEquals(0, uniffiContinuationHandleMap.size)
    }
    gate("future pending wake then success") {
        val future = MockFuture()
        val task = async { future.call() }
        waitUntil { future.polls.get() == 1 }
        future.wake(false)
        waitUntil { future.polls.get() == 2 }
        future.wake(true)
        assertEquals(42, task.await())
        assertEquals(0, future.cancels.get())
        assertEquals(1, future.frees.get())
    }
    gate("committed Ready survives cancellation of the awaiting Job") {
        val future = MockFuture()
        var delivered: Int? = null
        val task = launch { delivered = future.call() }
        waitUntil { future.polls.get() == 1 }
        future.wake(true)
        task.cancel()
        task.join()
        assertEquals(42, delivered)
        assertEquals(1, future.completes.get())
        assertEquals(1, future.frees.get())
    }
    gate("output null item, EOF, second collection rejected") {
        var polls = 0
        var closes = 0
        val stream = UniFfiStream<Int?>({
            when (polls++) {
                0 -> __UniffiStreamNext.Item(null)
                1 -> __UniffiStreamNext.Item(4)
                else -> __UniffiStreamNext.Done
            }
        }, { closes++ })
        assertEquals(listOf(null, 4), stream.toList())
        assertFailsWith<InternalException> { stream.toList() }
        stream.close()
        assertEquals(1, closes)
    }
    gate("output early exit and repeated close share cleanup") {
        val cleanup = CompletableDeferred<Unit>()
        val entered = CompletableDeferred<Unit>()
        var closes = 0
        val stream = UniFfiStream({ __UniffiStreamNext.Item(1) }, { closes++; entered.complete(Unit); cleanup.await() })
        val first = launch { stream.take(1).collect() }
        entered.await()
        val second = launch(start = CoroutineStart.UNDISPATCHED) { stream.close() }
        first.cancel()
        second.cancel()
        yield()
        assertFalse(first.isCompleted)
        assertFalse(second.isCompleted)
        cleanup.complete(Unit)
        joinAll(first, second)
        assertEquals(1, closes)
    }
    gate("input no prefetch and close waits producer finally") {
        val produced = AtomicInteger()
        val cleanup = CompletableDeferred<Unit>()
        val entered = CompletableDeferred<Unit>()
        val handle = input(flow {
            try {
                repeat(100) { produced.incrementAndGet(); emit(it) }
            } finally {
                withContext(NonCancellable) { entered.complete(Unit); cleanup.await() }
            }
        })
        assertEquals(0, produced.get())
        val first = next(handle).await()
        assertEquals(0, first.callStatus.code.toInt())
        delay(30)
        assertEquals(1, produced.get())
        val second = next(handle).await()
        assertEquals(0, second.callStatus.code.toInt())
        delay(30)
        assertEquals(2, produced.get())
        val closing = closeInput(handle)
        entered.await()
        val closingAgain = closeInput(handle)
        assertFalse(closing.isCompleted)
        assertFalse(closingAgain.isCompleted)
        cleanup.complete(Unit)
        assertEquals(0, closing.await().toInt())
        assertEquals(0, closingAgain.await().toInt())
        assertEquals(0, uniffiInputStreamHandleCountGate())
    }
    gate("input concurrent next rejected and cancellation preserves subscription") {
        val item = CompletableDeferred<Unit>()
        val handle = input(flow { item.await(); emit(8); emit(9) })
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        val pending = next(handle, dropped)
        val duplicate = next(handle).await()
        assertEquals(2, duplicate.callStatus.code.toInt())
        dropped.drop()
        assertEquals(3, pending.await().callStatus.code.toInt())
        item.complete(Unit)
        assertEquals(0, next(handle).await().callStatus.code.toInt())
        assertEquals(0, next(handle).await().callStatus.code.toInt())
        assertEquals(0, closeInput(handle).await().toInt())
    }
    gate("callback input reentry and single consumer") {
        val handle = input(flowOf(1, 2, 3))
        val lifted = uniffiLiftInputStream<Int>(handle)
        assertEquals(listOf(1, 2, 3), lifted.toList())
        assertFailsWith<InternalException> { lifted.toList() }
        assertEquals(0, closeInput(handle).await().toInt())
    }
    gate("foreign Job cancellation awaits NonCancellable finally and completes once") {
        val entered = CompletableDeferred<Unit>()
        val cleanup = CompletableDeferred<Unit>()
        val finalizing = CompletableDeferred<Unit>()
        val status = CompletableDeferred<Byte>()
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        uniffiTraitInterfaceCallAsync({
            try { entered.complete(Unit); awaitCancellation() }
            finally { withContext(NonCancellable) { finalizing.complete(Unit); cleanup.await() } }
        }, { _: Nothing -> error("unexpected success") }, { check(status.complete(it.code)) }, dropped)
        entered.await()
        dropped.drop()
        finalizing.await()
        assertFalse(status.isCompleted)
        cleanup.complete(Unit)
        assertEquals(3, status.await().toInt())
        assertEquals(0, uniffiForeignFutureHandleCount())
    }
    gate("input first business error is terminal") {
        val handle = input(flow { emit(2); throw IllegalArgumentException("stop") })
        assertEquals(0, next(handle).await().callStatus.code.toInt())
        assertEquals(1, next(handle).await().callStatus.code.toInt())
        val terminal = next(handle).await()
        assertEquals(0, terminal.callStatus.code.toInt())
        assertEquals(0, terminal.returnValue.bytes.get().toInt())
        assertEquals(0, closeInput(handle).await().toInt())
    }
    gate("output first business error closes and preserves error") {
        val expected = IllegalArgumentException("business")
        var pulls = 0
        var closes = 0
        val stream = UniFfiStream<Int>({ pulls++; __UniffiStreamNext.Error(expected) }, { closes++ })
        assertSame(expected, assertFailsWith<IllegalArgumentException> { stream.toList() })
        assertEquals(1, pulls)
        assertEquals(1, closes)
        assertFailsWith<InternalException> { stream.toList() }
    }
    gate("finalizer requests cleanup without claiming an early acknowledgement") {
        val entered = CompletableDeferred<Unit>()
        val release = CompletableDeferred<Unit>()
        val lifetime = UniffiStreamLifetime { entered.complete(Unit); release.await() }
        lifetime.run()
        entered.await()
        val closer = launch(start = CoroutineStart.UNDISPATCHED) { lifetime.close() }
        try { assertFalse(closer.isCompleted) } finally { release.complete(Unit) }
        closer.join()
    }
    gate("partial argument lowering releases unpublished input") {
        val count = uniffiInputStreamHandleCountGate()
        var starts = 0
        assertFailsWith<IllegalArgumentException> {
            uniffiWithInputStreamArguments { arguments ->
                arguments.add(input(flow { starts++; emit(1) }))
                throw IllegalArgumentException("later argument")
            }
        }
        assertEquals(count, uniffiInputStreamHandleCountGate())
        assertEquals(0, starts)
    }
    gate("input close before collect never starts producer") {
        var starts = 0
        val handle = input(flow { starts++; emit(1) })
        assertEquals(0, closeInput(handle).await().toInt())
        assertEquals(0, starts)
    }
    gate("output concurrent close prevents another pull") {
        val emitted = CompletableDeferred<Unit>()
        val resume = CompletableDeferred<Unit>()
        var pulls = 0
        val stream = UniFfiStream({ pulls++; __UniffiStreamNext.Item(1) }, {})
        val collector = launch {
            assertFailsWith<CancellationException> {
                stream.collect { emitted.complete(Unit); resume.await() }
            }
        }
        emitted.await()
        stream.close()
        resume.complete(Unit)
        collector.join()
        assertEquals(1, pulls)
    }
    gate("foreign cancellation before first scheduling still completes exactly once") {
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        val completions = AtomicInteger()
        var calls = 0
        val result = CompletableDeferred<Byte>()
        dropped.onSet = { dropped.drop() }
        uniffiTraitInterfaceCallAsync({ calls++; 7 }, { error("unexpected success") }, {
            completions.incrementAndGet()
            check(result.complete(it.code))
        }, dropped)
        assertEquals(3, result.await().toInt())
        assertEquals(0, calls)
        assertEquals(1, completions.get())
        assertEquals(0, uniffiForeignFutureHandleCount())
    }
    gate("foreign children finish before success callback and lease drops") {
        val entered = CompletableDeferred<Unit>()
        val finish = CompletableDeferred<Unit>()
        val result = CompletableDeferred<Int>()
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        uniffiTraitInterfaceCallAsync({
            coroutineScope { launch { entered.complete(Unit); finish.await() }; 7 }
        }, { check(result.complete(it)); dropped.drop() }, { error("unexpected ${it.code}") }, dropped)
        entered.await()
        assertFalse(result.isCompleted)
        finish.complete(Unit)
        assertEquals(7, result.await())
        waitUntil { uniffiForeignFutureHandleCount() == 0 }
    }
    for (viaAdapter in listOf(false, true)) {
        gate("uncollected owned input closes source, adapter=$viaAdapter") {
            val entered = CompletableDeferred<Unit>()
            val cleanup = CompletableDeferred<Unit>()
            var closes = 0
            val source = UniFfiStream<Int>({ error("must not collect") }, {
                closes++; entered.complete(Unit); cleanup.await()
            })
            val first = if (viaAdapter) input(source) else null
            val outer = input(if (first == null) source else uniffiLiftInputStream<Int>(first))
            val close = closeInput(outer)
            try {
                entered.await()
                assertFalse(close.isCompleted)
                cleanup.complete(Unit)
                assertEquals(0.toByte(), close.await())
                if (first != null) assertEquals(0.toByte(), closeInput(first).await())
                assertEquals(1, closes)
            } finally { cleanup.complete(Unit); source.close() }
        }
    }
    gate("cancelled foreign success disposes owned stream before unique completion") {
        val entered = CompletableDeferred<Unit>()
        val disposing = CompletableDeferred<Unit>()
        val cleanup = CompletableDeferred<Unit>()
        val completed = CompletableDeferred<Byte>()
        var job: Job? = null
        var closes = 0
        var completions = 0
        val source = UniFfiStream<Int>({ error("must not collect") }, {
            closes++; disposing.complete(Unit); cleanup.await()
        })
        val dropped = UniffiForeignFutureDroppedCallbackStruct()
        uniffiTraitInterfaceCallAsync({
            job = currentCoroutineContext().job
            entered.complete(Unit)
            try { awaitCancellation() } catch (_: CancellationException) { source }
        }, { error("must not deliver cancelled result") }, {
            completions++; completed.complete(it.code)
        }, dropped, discardResult = { uniffiStreamCloseAction(it)?.invoke() })
        try {
            entered.await()
            dropped.drop()
            disposing.await()
            assertFalse(completed.isCompleted)
            job!!.cancel()
            assertFalse(completed.isCompleted)
            cleanup.complete(Unit)
            assertEquals(3.toByte(), completed.await())
            assertEquals(1, closes)
            assertEquals(1, completions)
        } finally { cleanup.complete(Unit); source.close() }
    }
    assertEquals(0, uniffiInputStreamHandleCountGate())
    assertEquals(0, uniffiInputStreamNextJobHandleMap.size)
    assertEquals(0, uniffiContinuationHandleMap.size)
    println("$passed Kotlin JNA runtime gates passed")
}
