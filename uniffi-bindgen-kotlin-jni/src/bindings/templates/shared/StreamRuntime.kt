sealed class StreamStep<out T> {
    data class Item<T>(val value: T) : StreamStep<T>()
    data object Done : StreamStep<Nothing>()
}

class RustStream<T> internal constructor(
    private val pull: suspend () -> StreamStep<T>,
    private val release: suspend () -> Unit,
    discard: () -> Unit,
) : kotlinx.coroutines.flow.Flow<T> {
    private class Cleanup(private val action: () -> Unit) : Runnable { override fun run() { action() } }
    private val cleanable = CLEANER.register(this, Cleanup(discard))
    private val collected = java.util.concurrent.atomic.AtomicBoolean(false)
    private val pulling = java.util.concurrent.atomic.AtomicBoolean(false)
    private val closing = java.util.concurrent.atomic.AtomicBoolean(false)
    private val closed = kotlinx.coroutines.CompletableDeferred<Unit>()

    suspend fun next(): StreamStep<T> {
        check(pulling.compareAndSet(false, true)) { "concurrent stream next() is not supported" }
        try {
            if (closing.get()) { closed.await(); return StreamStep.Done }
            return pull()
        } catch (error: Throwable) {
            close()
            throw error
        } finally { pulling.set(false) }
    }

    suspend fun close() = kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) {
        if (closing.compareAndSet(false, true)) {
            try { release(); cleanable.clean(); closed.complete(Unit) }
            catch (error: Throwable) { closed.completeExceptionally(error) }
        }
        closed.await()
    }

    @OptIn(kotlinx.coroutines.InternalCoroutinesApi::class)
    override suspend fun collect(collector: kotlinx.coroutines.flow.FlowCollector<T>) {
        check(collected.compareAndSet(false, true)) { "Rust stream can only be collected once" }
        try {
            while (true) {
                when (val step = next()) {
                    is StreamStep.Item -> collector.emit(step.value)
                    StreamStep.Done -> return
                }
            }
        } finally { close() }
    }
}

suspend fun kotlinx.coroutines.flow.Flow<*>.close() {
    (this as RustStream<*>).close()
}

// Each lowered producer owns a Job and two rendezvous channels. An emit does
// not return until the consumer asks for its next item, preventing prefetch.
internal class UniffiInputStream<T>(private val flow: kotlinx.coroutines.flow.Flow<T>) {
    private val requests = kotlinx.coroutines.channels.Channel<Unit>(0)
    private val items = kotlinx.coroutines.channels.Channel<Result<StreamStep<T>>>(0)
    private val owner = kotlinx.coroutines.SupervisorJob()
    private val pulling = java.util.concurrent.atomic.AtomicBoolean(false)
    private val closing = java.util.concurrent.atomic.AtomicBoolean(false)
    private val closed = kotlinx.coroutines.CompletableDeferred<Unit>()
    private val job = kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Default + owner).launch(
        start = kotlinx.coroutines.CoroutineStart.LAZY,
    ) {
        try {
            requests.receive()
            flow.collect { value ->
                items.send(Result.success(StreamStep.Item(value)))
                requests.receive()
            }
            items.send(Result.success(StreamStep.Done))
        } catch (cancelled: kotlin.coroutines.cancellation.CancellationException) {
            throw cancelled
        } catch (error: Throwable) {
            items.send(Result.failure(error))
        } finally { items.close() }
    }

    suspend fun next(): StreamStep<T> {
        check(pulling.compareAndSet(false, true)) { "concurrent input next() is not supported" }
        try {
            job.start()
            requests.send(Unit)
            return items.receive().getOrThrow()
        } finally { pulling.set(false) }
    }
    fun cancel() { job.cancel() }
    suspend fun close() = kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) {
        if (closing.compareAndSet(false, true)) {
            try {
                job.cancel()
                job.join()
                uniffiCloseFlow(flow)
                requests.cancel()
                items.cancel()
                owner.complete()
                closed.complete(Unit)
            } catch (error: Throwable) { closed.completeExceptionally(error) }
        }
        closed.await()
    }
}


internal suspend fun uniffiCloseFlow(flow: kotlinx.coroutines.flow.Flow<*>) {
    when (flow) {
        is RustStream<*> -> flow.close()
        is UniffiOwnedFlow<*> -> flow.close()
    }
}

internal class UniffiOwnedFlow<T>(
    private val flow: kotlinx.coroutines.flow.Flow<T>,
    private val cleanup: suspend () -> Unit,
) : kotlinx.coroutines.flow.Flow<T> {
    @OptIn(kotlinx.coroutines.InternalCoroutinesApi::class)
    override suspend fun collect(collector: kotlinx.coroutines.flow.FlowCollector<T>) = flow.collect(collector)
    suspend fun close() { uniffiCloseFlow(flow); cleanup() }
}
