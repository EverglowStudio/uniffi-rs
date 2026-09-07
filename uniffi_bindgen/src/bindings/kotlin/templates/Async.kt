// Async return type handlers

internal const val UNIFFI_RUST_FUTURE_POLL_READY = 0.toByte()
internal const val UNIFFI_RUST_FUTURE_POLL_WAKE = 1.toByte()

// The Rust output-stream ABI completes one strict tagged StreamStep as a
// RustBuffer. `Item(null)` is an item payload when T is nullable; only a
// distinct Done tag completes the stream, and Error keeps the typed payload.
internal sealed class __UniffiStreamNext<out T> {
    class Item<T>(val value: T) : __UniffiStreamNext<T>()
    object Done : __UniffiStreamNext<Nothing>()
    class Error(val error: Throwable) : __UniffiStreamNext<Nothing>()
}

private fun <T> __uniffiLiftStreamNext(
    rustBuffer: RustBuffer.ByValue,
    readItem: (ByteBuffer) -> T,
    readError: (ByteBuffer) -> Throwable,
): __UniffiStreamNext<T> {
    try {
        val buffer = rustBuffer.asByteBuffer()
            ?: throw InternalException("stream next returned an empty RustBuffer")
        return when (buffer.get().toInt()) {
            1 -> {
                val item = readItem(buffer)
                if (buffer.hasRemaining()) {
                    throw InternalException("stream Item result contains trailing bytes")
                }
                __UniffiStreamNext.Item(item)
            }
            2 -> {
                if (buffer.hasRemaining()) {
                    throw InternalException("stream Done result contains trailing bytes")
                }
                __UniffiStreamNext.Done
            }
            3 -> {
                val error = readError(buffer)
                if (buffer.hasRemaining()) {
                    throw InternalException("stream Error result contains trailing bytes")
                }
                __UniffiStreamNext.Error(error)
            }
            else -> throw InternalException("unexpected output stream step tag")
        }
    } finally {
        RustBuffer.free(rustBuffer)
    }
}

{%- if ci.has_stream_fns() %}

private class UniffiStreamLifetime(private val closeSubscription: suspend () -> Unit): Runnable {
    val closing = AtomicBoolean(false)
    private val closed = CompletableDeferred<Unit>()

    private suspend fun finishClose() {
        try {
            closeSubscription()
            closed.complete(Unit)
        } catch (error: Throwable) {
            closed.completeExceptionally(error)
        }
    }

    suspend fun close() {
        withContext(NonCancellable) {
            if (closing.compareAndSet(false, true)) finishClose()
            closed.await()
        }
    }

    override fun run() {
        // Finalization requests cleanup but cannot acknowledge it synchronously.
        // The action owns only the subscription functions, never the Flow wrapper.
        if (closing.compareAndSet(false, true)) {
            CoroutineScope(Dispatchers.Default).launch { finishClose() }
        }
    }
}

/** A Rust subscription. Collection is single-use; close waits for owned cleanup. */
public class UniFfiStream<T> internal constructor(
    private val next: suspend () -> __UniffiStreamNext<T>,
    closeSubscription: suspend () -> Unit,
) : Flow<T> {
    private val consumed = AtomicBoolean(false)
    private val lifetime = UniffiStreamLifetime(closeSubscription)
    private val cleanable = UniffiLib.CLEANER.register(this, lifetime)

    public suspend fun close() {
        try {
            lifetime.close()
        } finally {
            cleanable.clean()
        }
    }

    @OptIn(kotlinx.coroutines.InternalCoroutinesApi::class)
    override suspend fun collect(collector: FlowCollector<T>) {
        if (!consumed.compareAndSet(false, true)) {
            throw InternalException("UniFFI output streams may only be consumed once")
        }
        if (lifetime.closing.get()) throw InternalException("UniFFI output stream is closed")
        var failure: Throwable? = null
        try {
            while (true) {
                currentCoroutineContext().ensureActive()
                if (lifetime.closing.get()) throw CancellationException("UniFFI output stream is closed")
                when (val step = next()) {
                    is __UniffiStreamNext.Item -> collector.emit(step.value)
                    __UniffiStreamNext.Done -> break
                    is __UniffiStreamNext.Error -> throw step.error
                }
            }
        } catch (error: Throwable) {
            failure = error
            throw error
        } finally {
            try {
                close()
            } catch (cleanupError: Throwable) {
                val originalError = failure
                if (originalError == null) throw cleanupError
                originalError.addSuppressed(cleanupError)
            }
        }
    }
}

{%- endif %}

// Poll callbacks outlive cancellation of the Kotlin waiter. Keep the outstanding
// callback registered until Rust invokes it, then continue polling cleanup.
internal val uniffiContinuationHandleMap = UniffiHandleMap<CompletableDeferred<Byte>>()

internal object uniffiRustFutureContinuationCallbackImpl: UniffiRustFutureContinuationCallback {
    override fun callback(data: Long, pollResult: Byte) {
        uniffiContinuationHandleMap.remove(data).complete(pollResult)
    }
}

internal suspend fun<T, F, E: kotlin.Exception> uniffiRustCallAsync(
    rustFuture: Long,
    pollFunc: (Long, UniffiRustFutureContinuationCallback, Long) -> Unit,
    cancelFunc: (Long) -> Unit,
    completeFunc: (Long, UniffiRustCallStatus) -> F,
    freeFunc: (Long) -> Unit,
    liftFunc: (F) -> T,
    errorHandler: UniffiRustCallStatusErrorHandler<E>,
    discardResult: suspend (T) -> Unit = {},
): T {
    var pendingPoll: CompletableDeferred<Byte>? = null
    var ready = false
    var pollingFailure: Throwable? = null
    fun poll(): CompletableDeferred<Byte> {
        val result = CompletableDeferred<Byte>()
        val handle = uniffiContinuationHandleMap.insert(result)
        try {
            pollFunc(rustFuture, uniffiRustFutureContinuationCallbackImpl, handle)
        } catch (error: Throwable) {
            uniffiContinuationHandleMap.remove(handle)
            throw error
        }
        return result
    }
    try {
        do {
            currentCoroutineContext().ensureActive()
            pendingPoll = poll()
            ready = pendingPoll.await() == UNIFFI_RUST_FUTURE_POLL_READY
            pendingPoll = null
        } while (!ready)
    } catch (error: Throwable) {
        pollingFailure = error
    }
    // Cancellation requests a Rust terminal decision; it does not replace one
    // that Rust already committed. Keep this dispatcher for result delivery:
    // withContext(NonCancellable) then has no cancellable dispatch back.
    return withContext(NonCancellable) {
        var callFailure: Throwable? = null
        try {
            if (!ready) {
                cancelFunc(rustFuture)
                // Do not replace a continuation that Rust still owns.
                var result = pendingPoll?.await()
                while (result != UNIFFI_RUST_FUTURE_POLL_READY) {
                    result = poll().await()
                }
            }
            val value = liftFunc(uniffiRustCallWithError(errorHandler) { status ->
                completeFunc(rustFuture, status)
            })
            val failure = pollingFailure
            if (failure != null && failure !is CancellationException) {
                try {
                    discardResult(value)
                } catch (cleanupError: Throwable) {
                    failure.addSuppressed(cleanupError)
                }
                throw failure
            }
            value
        } catch (error: Throwable) {
            callFailure = error
            throw error
        } finally {
            try {
                freeFunc(rustFuture)
            } catch (cleanupError: Throwable) {
                val originalError = callFailure
                if (originalError == null) throw cleanupError
                originalError.addSuppressed(cleanupError)
            }
        }
    }
}

{%- if ci.has_async_callback_interface_definition() %}
internal fun<T> uniffiTraitInterfaceCallAsync(
    makeCall: suspend () -> T,
    handleSuccess: (T) -> Unit,
    handleError: (UniffiRustCallStatus.ByValue) -> Unit,
    uniffiOutDroppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    discardResult: suspend (T) -> Unit = {},
) = uniffiLaunchForeignFuture(makeCall, handleSuccess, handleError, { error ->
    UniffiRustCallStatus.create(
        if (error is CancellationException) 3.toByte() else UNIFFI_CALL_UNEXPECTED_ERROR,
        if (error is CancellationException) RustBuffer.ByValue() else {{ Type::String.borrow()|lower_fn }}(error.toString()),
    )
}, uniffiOutDroppedCallback, discardResult)

internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallAsyncWithError(
    noinline makeCall: suspend () -> T,
    noinline handleSuccess: (T) -> Unit,
    noinline handleError: (UniffiRustCallStatus.ByValue) -> Unit,
    crossinline lowerError: (E) -> RustBuffer.ByValue,
    uniffiOutDroppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    noinline discardResult: suspend (T) -> Unit = {},
) = uniffiLaunchForeignFuture(makeCall, handleSuccess, handleError, { error ->
    when (error) {
        is CancellationException -> UniffiRustCallStatus.create(3.toByte(), RustBuffer.ByValue())
        is E -> UniffiRustCallStatus.create(UNIFFI_CALL_ERROR, lowerError(error))
        else -> UniffiRustCallStatus.create(UNIFFI_CALL_UNEXPECTED_ERROR, {{ Type::String.borrow()|lower_fn }}(error.toString()))
    }
}, uniffiOutDroppedCallback, discardResult)

internal fun<T> uniffiLaunchForeignFuture(
    makeCall: suspend () -> T,
    handleSuccess: (T) -> Unit,
    handleError: (UniffiRustCallStatus.ByValue) -> Unit,
    errorStatus: (Throwable) -> UniffiRustCallStatus.ByValue,
    droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    discardResult: suspend (T) -> Unit,
) {
    // The Rust future owns this Job. Publish its cancellation handle before it
    // can execute, and deliver completion only after the Job and its children
    // have finished (including suspending NonCancellable finally blocks).
    val owner = Job()
    var result: Result<T>? = null
    val scope = CoroutineScope(owner + Dispatchers.Default)
    val job = scope.launch(start = CoroutineStart.LAZY) {
        result = runCatching { makeCall() }
    }
    val handle = uniffiForeignFutureHandleMap.insert(job)
    droppedCallback.uniffiSetValue(UniffiForeignFutureDroppedCallbackStruct(handle, uniffiForeignFutureDroppedCallbackImpl))
    job.invokeOnCompletion { cause ->
        val completed = result
        result = null
        try {
            if (cause != null && completed?.isSuccess == true) {
                // The cancelled body may catch cancellation and return an owned
                // value. A sibling owned by this call disposes it before ack;
                // the cancelled body Job cannot interrupt this cleanup.
                scope.launch(start = CoroutineStart.UNDISPATCHED) {
                    withContext(NonCancellable) {
                        try {
                            discardResult(completed.getOrThrow())
                        } catch (cleanupError: Throwable) {
                            cause.addSuppressed(cleanupError)
                        }
                        handleError(errorStatus(cause))
                    }
                }
            } else if (cause != null) {
                handleError(errorStatus(cause))
            } else if (completed != null) {
                completed.fold(handleSuccess) { handleError(errorStatus(it)) }
            } else {
                handleError(errorStatus(InternalException("Foreign future ended without a result")))
            }
        } finally {
            owner.complete()
        }
    }
    job.start()
}

internal val uniffiForeignFutureHandleMap = UniffiHandleMap<Job>()

internal object uniffiForeignFutureDroppedCallbackImpl: UniffiForeignFutureDroppedCallback {
    override fun callback(handle: Long) {
        val job = uniffiForeignFutureHandleMap.remove(handle)
        if (!job.isCompleted) {
            job.cancel()
        }
    }
}

// For testing
public fun uniffiForeignFutureHandleCount() = uniffiForeignFutureHandleMap.size

{%- endif %}
