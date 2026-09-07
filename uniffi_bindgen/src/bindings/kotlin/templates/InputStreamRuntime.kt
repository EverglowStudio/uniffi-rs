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

private class UniffiFlowInputStream<T>(
    flow: Flow<T>,
    private val lowerNext: (UniffiInputStreamNext<T>) -> RustBuffer.ByValue,
    private val lowerError: (Throwable) -> RustBuffer.ByValue?,
): UniffiInputStreamProtocol {
    private val lock = Any()
    private var flow: Flow<T>? = flow
    private val channel = Channel<T>(Channel.RENDEZVOUS)
    private val scopeJob = SupervisorJob()
    private val scope = CoroutineScope(scopeJob + Dispatchers.Default)
    private var producerJob: Job? = null
    private var pendingJob: UniffiInputStreamNextJobBox? = null
    private val nextCompletions = mutableSetOf<kotlinx.coroutines.CompletableDeferred<Unit>>()
    private var done = false
    private var cancelled = false

    override fun next(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    ) {
        // Register every child before close can cancel and join the scope.
        synchronized(lock) {
            if (cancelled || done) {
                uniffiCompleteInputStreamSuccess(
                    UniffiInputStreamNext.Done, lowerNext, callback, callbackData,
                )
                return
            }
            if (pendingJob != null) {
                uniffiCompleteInputStreamUnexpected(
                    "UniFFI input stream received a concurrent next() request", callback, callbackData,
                )
                return
            }
            val nextJob = UniffiInputStreamNextJobBox()
            pendingJob = nextJob
            val nextJobHandle = uniffiInputStreamNextJobHandleMap.insert(nextJob)
            droppedCallback.uniffiSetValue(UniffiForeignFutureDroppedCallbackStruct(
                nextJobHandle, uniffiInputStreamNextDroppedCallbackImpl,
            ))
            val completion = kotlinx.coroutines.CompletableDeferred<Unit>()
            nextCompletions.add(completion)
            val completed = java.util.concurrent.atomic.AtomicBoolean(false)
            val completeOnce = object: UniffiForeignFutureCompleteRustBuffer {
                override fun callback(callbackData: Long, result: UniffiForeignFutureResultRustBuffer.UniffiByValue) {
                    completed.set(true)
                    callback.callback(callbackData, result)
                }
            }
            val job = scope.launch(start = kotlinx.coroutines.CoroutineStart.LAZY) {
                runNext(completeOnce, callbackData)
            }
            nextJob.set(job)
            job.invokeOnCompletion { cause ->
                try {
                    synchronized(lock) {
                        if (pendingJob === nextJob) pendingJob = null
                    }
                    try {
                        uniffiInputStreamNextJobHandleMap.remove(nextJobHandle)
                    } catch (_: InternalException) {
                    }
                    // A lazy child can be cancelled before runNext's body starts.
                    if (completed.compareAndSet(false, true)) {
                        if (cause is CancellationException) {
                            uniffiCompleteInputStream(
                                RustBuffer.ByValue(),
                                UniffiRustCallStatus.create(3.toByte(), RustBuffer.ByValue()),
                                callback, callbackData,
                            )
                        } else {
                            uniffiCompleteInputStreamUnexpected(
                                cause ?: InternalException("UniFFI input stream next ended without a result"),
                                callback, callbackData,
                            )
                        }
                    }
                } finally {
                    synchronized(lock) {
                        completion.complete(Unit)
                        nextCompletions.remove(completion)
                    }
                }
            }
            ensureProducerStartedLocked()
            job.start()
        }
    }

    override fun cancel() {
        synchronized(lock) {
            cancelled = true
            done = true
            pendingJob?.cancel()
            scopeJob.cancel()
            channel.cancel()
        }
    }

    override suspend fun close() {
        cancel()
        scopeJob.join()
        // Job completion handlers can still be delivering the final FFI callback.
        val completions = synchronized(lock) { nextCompletions.toList() }
        for (completion in completions) completion.await()
        synchronized(lock) {
            flow = null
            producerJob = null
            pendingJob = null
        }
    }

    private fun ensureProducerStartedLocked() {
        if (producerJob != null) {
            return
        }
        producerJob = scope.launch {
            try {
                flow!!.collect { value ->
                    channel.send(value)
                }
                channel.close()
            } catch (e: Throwable) {
                channel.close(e)
            }
        }
    }

    private suspend fun runNext(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
    ) {
        try {
            val result = channel.receiveCatching()
            if (result.isSuccess) {
                finishNext(done = false)
                uniffiCompleteInputStreamSuccess(
                    UniffiInputStreamNext.Item(result.getOrThrow()),
                    lowerNext,
                    callback,
                    callbackData,
                )
                return
            }

            finishNext(done = true)
            val error = result.exceptionOrNull()
            if (error == null) {
                uniffiCompleteInputStreamSuccess(
                    UniffiInputStreamNext.Done,
                    lowerNext,
                    callback,
                    callbackData,
                )
            } else {
                uniffiCompleteInputStreamError(error, lowerError, callback, callbackData)
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Throwable) {
            finishNext(done = true)
            uniffiCompleteInputStreamError(e, lowerError, callback, callbackData)
        }
    }

    private fun finishNext(done: Boolean) {
        synchronized(lock) {
            pendingJob = null
            if (done) {
                this.done = true
            }
        }
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
