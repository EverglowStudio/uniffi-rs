internal sealed class UniffiInputStreamNext<out T> {
    data class Item<T>(val value: T): UniffiInputStreamNext<T>()
    object Done: UniffiInputStreamNext<Nothing>()
}

private interface UniffiInputStreamProtocol {
    fun next(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    )

    fun cancel()
    suspend fun close()
    fun asFlow(): Flow<*>
}

private class UniffiInputStreamNextJobBox {
    private val lock = Any()
    private var job: Job? = null
    private var cancelled = false

    fun set(job: Job) {
        val cancel = synchronized(lock) {
            this.job = job
            cancelled
        }
        if (cancel) job.cancel()
    }

    fun cancel() {
        val job = synchronized(lock) {
            cancelled = true
            this.job
        }
        // Job completion handlers may acquire the stream lock.
        job?.cancel()
    }
}

private val uniffiInputStreamHandleMap = UniffiHandleMap<UniffiInputStreamProtocol>()
private val uniffiInputStreamNextJobHandleMap = UniffiHandleMap<UniffiInputStreamNextJobBox>()

// Preserve close ownership when an input travels through a foreign callback
// without ever being collected. An ordinary cold Flow has no close operation.
private class UniffiInputStreamFlow<T>(
    private val owner: UniffiInputStreamProtocol,
    private val source: Flow<T>,
): Flow<T> {
    suspend fun close() = owner.close()

    @OptIn(kotlinx.coroutines.InternalCoroutinesApi::class)
    override suspend fun collect(collector: FlowCollector<T>) = source.collect(collector)
}

private fun uniffiStreamCloseAction(source: Flow<*>): (suspend () -> Unit)? = when (source) {
    {%- if ci.has_stream_fns() %}
    is UniFfiStream<*> -> source::close
    {%- endif %}
    is UniffiInputStreamFlow<*> -> source::close
    else -> null
}

private class UniffiFlowInputStream<T>(
    flow: Flow<T>,
    private val lowerNext: (UniffiInputStreamNext<T>) -> RustBuffer.ByValue,
    private val lowerError: (Throwable) -> RustBuffer.ByValue?,
): UniffiInputStreamProtocol {
    private val lock = Any()
    private var flow: Flow<T>? = flow
    private val closeSource = uniffiStreamCloseAction(flow)
    private val channel = Channel<T>(Channel.RENDEZVOUS)
    // Returning from emit would let a Flow compute another value. Keep emit
    // suspended until the next pull explicitly authorizes that work.
    private val demand = Channel<Unit>(1)
    private var needsDemand = false
    private val scopeJob = SupervisorJob()
    private val scope = CoroutineScope(scopeJob + Dispatchers.Default)
    private var producerJob: Job? = null
    private val rustConsumer = Any()
    private var consumer: Any? = null
    private var pending = false
    private val nextCompletions = mutableSetOf<CompletableDeferred<Unit>>()
    private var done = false
    private var cancelled = false

    private fun reserveLocked(owner: Any): CompletableDeferred<Unit> {
        if (consumer == null) consumer = owner
        if (consumer !== owner) throw InternalException("UniFFI input streams may only be consumed once")
        if (pending) throw InternalException("UniFFI input stream received a concurrent next() request")
        pending = true
        return CompletableDeferred<Unit>().also { nextCompletions.add(it) }
    }

    private fun release(completion: CompletableDeferred<Unit>) {
        synchronized(lock) { nextCompletions.remove(completion) }
        // Resuming an Unconfined waiter can synchronously reenter foreign code.
        completion.complete(Unit)
    }

    override fun next(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    ) {
        val job: Job
        val completion: CompletableDeferred<Unit>
        val nextJobHandle: Long
        var result: Result<UniffiInputStreamNext<T>>? = null
        try {
            synchronized(lock) {
                completion = reserveLocked(rustConsumer)
                val nextJob = UniffiInputStreamNextJobBox()
                nextJobHandle = uniffiInputStreamNextJobHandleMap.insert(nextJob)
                droppedCallback.uniffiSetValue(UniffiForeignFutureDroppedCallbackStruct(
                    nextJobHandle, uniffiInputStreamNextDroppedCallbackImpl,
                ))
                job = scope.launch(start = CoroutineStart.LAZY) {
                    result = runCatching { pull() }
                }
                nextJob.set(job)
            }
        } catch (error: InternalException) {
            uniffiCompleteInputStreamUnexpected(error, callback, callbackData)
            return
        }
        job.invokeOnCompletion { cause ->
            synchronized(lock) { pending = false }
            try {
                try {
                    uniffiInputStreamNextJobHandleMap.remove(nextJobHandle)
                } catch (_: InternalException) {
                    // Rust already dropped its next waiter.
                }
                val completed = result
                val failure = cause ?: completed?.exceptionOrNull()
                when {
                    failure is CancellationException -> uniffiCompleteInputStream(
                        RustBuffer.ByValue(),
                        UniffiRustCallStatus.create(3.toByte(), RustBuffer.ByValue()),
                        callback, callbackData,
                    )
                    failure != null -> uniffiCompleteInputStreamError(failure, lowerError, callback, callbackData)
                    completed != null -> uniffiCompleteInputStreamSuccess(completed.getOrThrow(), lowerNext, callback, callbackData)
                    else -> uniffiCompleteInputStreamUnexpected("Input stream next ended without a result", callback, callbackData)
                }
            } finally {
                release(completion)
            }
        }
        // No registry/state lock is held when starting work or calling foreign code.
        job.start()
    }

    override fun asFlow(): Flow<T> = UniffiInputStreamFlow(this, kotlinx.coroutines.flow.flow {
        val owner = Any()
        var claimed = false
        try {
            while (true) {
                val completion = synchronized(lock) { reserveLocked(owner) }
                claimed = true
                val step = try {
                    pull()
                } finally {
                    synchronized(lock) { pending = false }
                    release(completion)
                }
                when (step) {
                    is UniffiInputStreamNext.Item -> emit(step.value)
                    UniffiInputStreamNext.Done -> break
                }
            }
        } finally {
            if (claimed) close()
        }
    })

    override fun cancel() {
        synchronized(lock) {
            cancelled = true
            flow = null
        }
        // Cancelling Jobs can run completion callbacks, including reentrant FFI.
        scopeJob.cancel()
        channel.cancel()
        demand.cancel()
    }

    override suspend fun close() {
        withContext(NonCancellable) {
            this@UniffiFlowInputStream.cancel()
            scopeJob.join()
            val completions = synchronized(lock) { nextCompletions.toList() }
            for (completion in completions) completion.await()
            synchronized(lock) { producerJob = null }
            closeSource?.invoke()
        }
    }

    private suspend fun pull(): UniffiInputStreamNext<T> {
        val producer: Job
        synchronized(lock) {
            if (cancelled) throw CancellationException("UniFFI input stream is closed")
            if (done) return UniffiInputStreamNext.Done
            producer = producerJob ?: run {
                val source = flow ?: throw InternalException("UniFFI input stream is closed")
                scope.launch(start = CoroutineStart.LAZY) {
                    try {
                        source.collect { value ->
                            channel.send(value)
                            demand.receive()
                        }
                        channel.close()
                    } catch (error: Throwable) {
                        channel.close(error)
                    }
                }.also { producerJob = it }
            }
            if (needsDemand) {
                needsDemand = false
                check(demand.trySend(Unit).isSuccess)
            }
        }
        producer.start()
        val result = channel.receiveCatching()
        if (result.isSuccess) {
            synchronized(lock) { needsDemand = true }
            return UniffiInputStreamNext.Item(result.getOrThrow())
        }
        synchronized(lock) { done = true }
        result.exceptionOrNull()?.let { throw it }
        return UniffiInputStreamNext.Done
    }
}

@Suppress("UNCHECKED_CAST") // The generated converter fixes the item type for this opaque handle.
private fun<T> uniffiLiftInputStream(handle: Long): Flow<T> =
    uniffiInputStreamHandleMap.get(handle).asFlow() as Flow<T>

// Argument lowering can fail before FFI takes ownership. These producers have
// never been published to Rust or started, so abandoning them needs no await.
private class UniffiInputStreamArguments {
    private val handles = mutableListOf<Long>()
    fun add(handle: Long) { handles.add(handle) }
    fun transfer() { handles.clear() }
    fun discard() {
        for (handle in handles) uniffiInputStreamHandleMap.remove(handle).cancel()
    }
}

private inline fun<R> uniffiWithInputStreamArguments(block: (UniffiInputStreamArguments) -> R): R {
    val arguments = UniffiInputStreamArguments()
    try {
        return block(arguments)
    } finally {
        arguments.discard()
    }
}

private fun<T> uniffiCreateInputStream(
    flow: Flow<T>,
    lowerNext: (UniffiInputStreamNext<T>) -> RustBuffer.ByValue,
    lowerError: (Throwable) -> RustBuffer.ByValue?,
): Long =
    uniffiInputStreamHandleMap.insert(
        UniffiFlowInputStream(
            flow,
            lowerNext,
            lowerError,
        )
    )

private object uniffiInputStreamNextCallbackImpl: UniffiInputStreamNextCallback {
    override fun callback(
        handle: Long,
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    ) {
        try {
            uniffiInputStreamHandleMap.get(handle).next(
                callback,
                callbackData,
                droppedCallback,
            )
        } catch (_: InternalException) {
            uniffiCompleteInputStreamUnexpected(
                "UniFFI input stream handle is no longer registered",
                callback,
                callbackData,
            )
        }
    }
}

private object uniffiInputStreamCancelCallbackImpl: UniffiInputStreamCancelCallback {
    override fun callback(handle: Long) {
        try {
            uniffiInputStreamHandleMap.get(handle).cancel()
        } catch (_: InternalException) {
        }
    }
}

private object uniffiInputStreamCloseCallbackImpl: UniffiInputStreamCloseCallback {
    override fun callback(
        handle: Long,
        callback: UniffiForeignFutureCompleteVoid,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    ) {
        // The stream owns cleanup; dropping a Rust waiter only requests cancellation.
        droppedCallback.uniffiSetValue(UniffiForeignFutureDroppedCallbackStruct(
            handle, uniffiInputStreamCloseDroppedCallbackImpl,
        ))
        val stream = try {
            uniffiInputStreamHandleMap.get(handle)
        } catch (_: InternalException) {
            uniffiCompleteInputStreamClose(callback, callbackData, UniffiRustCallStatus.create(
                UNIFFI_CALL_UNEXPECTED_ERROR,
                {{ Type::String.borrow()|lower_fn }}("UniFFI input stream handle is no longer registered"),
            ))
            return
        }
        CoroutineScope(Dispatchers.Default).launch {
            val status = try {
                stream.close()
                try {
                    uniffiInputStreamHandleMap.remove(handle)
                } catch (_: InternalException) {
                }
                UniffiRustCallStatus.ByValue()
            } catch (e: Throwable) {
                UniffiRustCallStatus.create(UNIFFI_CALL_UNEXPECTED_ERROR,
                    {{ Type::String.borrow()|lower_fn }}(e.toString()))
            }
            uniffiCompleteInputStreamClose(callback, callbackData, status)
        }
    }
}

private object uniffiInputStreamCloseDroppedCallbackImpl: UniffiForeignFutureDroppedCallback {
    override fun callback(handle: Long) {
        uniffiInputStreamCancelCallbackImpl.callback(handle)
    }
}

private fun uniffiCompleteInputStreamClose(
    callback: UniffiForeignFutureCompleteVoid,
    callbackData: Long,
    status: UniffiRustCallStatus.ByValue,
) {
    val result = UniffiForeignFutureResultVoid.UniffiByValue(status)
    result.write()
    callback.callback(callbackData, result)
}

private object uniffiInputStreamNextDroppedCallbackImpl: UniffiForeignFutureDroppedCallback {
    override fun callback(handle: Long) {
        try {
            uniffiInputStreamNextJobHandleMap.remove(handle).cancel()
        } catch (_: InternalException) {
        }
    }
}

private fun<T> uniffiCompleteInputStreamSuccess(
    next: UniffiInputStreamNext<T>,
    lowerNext: (UniffiInputStreamNext<T>) -> RustBuffer.ByValue,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: Long,
) {
    val returnValue = try {
        lowerNext(next)
    } catch (e: Throwable) {
        uniffiCompleteInputStreamUnexpected(e, callback, callbackData)
        return
    }
    uniffiCompleteInputStream(
        returnValue,
        UniffiRustCallStatus.ByValue(),
        callback,
        callbackData,
    )
}

private fun uniffiCompleteInputStreamError(
    error: Throwable,
    lowerError: (Throwable) -> RustBuffer.ByValue?,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: Long,
) {
    val errorBuf = try {
        lowerError(error)
    } catch (e: Throwable) {
        uniffiCompleteInputStreamUnexpected(e, callback, callbackData)
        return
    }

    if (errorBuf == null) {
        uniffiCompleteInputStreamUnexpected(error, callback, callbackData)
    } else {
        uniffiCompleteInputStream(
            RustBuffer.ByValue(),
            UniffiRustCallStatus.create(UNIFFI_CALL_ERROR, errorBuf),
            callback,
            callbackData,
        )
    }
}

private fun uniffiCompleteInputStreamUnexpected(
    error: Throwable,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: Long,
) {
    val message = try {
        error.stackTraceToString()
    } catch (_: Throwable) {
        error.toString()
    }
    uniffiCompleteInputStreamUnexpected(message, callback, callbackData)
}

private fun uniffiCompleteInputStreamUnexpected(
    message: String,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: Long,
) {
    uniffiCompleteInputStream(
        RustBuffer.ByValue(),
        UniffiRustCallStatus.create(
            UNIFFI_CALL_UNEXPECTED_ERROR,
            {{ Type::String.borrow()|lower_fn }}(message),
        ),
        callback,
        callbackData,
    )
}

private fun uniffiCompleteInputStream(
    returnValue: RustBuffer.ByValue,
    callStatus: UniffiRustCallStatus.ByValue,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: Long,
) {
    val result = UniffiForeignFutureResultRustBuffer.UniffiByValue(returnValue, callStatus)
    result.write()
    callback.callback(callbackData, result)
}

// For testing
public fun uniffiInputStreamHandleCount{{ ci.namespace()|class_name(ci) }}(): Int =
    uniffiInputStreamHandleMap.size
