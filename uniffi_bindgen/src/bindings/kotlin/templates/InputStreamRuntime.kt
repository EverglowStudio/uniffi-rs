internal sealed class UniffiInputStreamNext<out T> {
    data class Item<T>(val value: T): UniffiInputStreamNext<T>()
    object Done: UniffiInputStreamNext<Nothing>()
}

private enum class UniffiInputStreamNextStart {
    START,
    DONE,
    CONCURRENT,
}

private interface UniffiInputStreamProtocol {
    fun next(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    )

    fun cancel()
}

private class UniffiInputStreamNextJobBox {
    private val lock = Any()
    private var job: Job? = null

    fun set(job: Job) {
        synchronized(lock) {
            this.job = job
        }
    }

    fun cancel() {
        synchronized(lock) {
            job?.cancel()
        }
    }
}

private val uniffiInputStreamHandleMap = UniffiHandleMap<UniffiInputStreamProtocol>()
private val uniffiInputStreamNextJobHandleMap = UniffiHandleMap<UniffiInputStreamNextJobBox>()

private class UniffiFlowInputStream<T>(
    private val flow: Flow<T>,
    private val lowerNext: (UniffiInputStreamNext<T>) -> RustBuffer.ByValue,
    private val lowerError: (Throwable) -> RustBuffer.ByValue?,
): UniffiInputStreamProtocol {
    private val lock = Any()
    private val channel = Channel<T>(Channel.RENDEZVOUS)
    private val scopeJob = SupervisorJob()
    private val scope = CoroutineScope(scopeJob + Dispatchers.Default)
    private var producerJob: Job? = null
    private var pendingJob: UniffiInputStreamNextJobBox? = null
    private var done = false
    private var cancelled = false

    override fun next(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: Long,
        droppedCallback: UniffiForeignFutureDroppedCallbackStruct,
    ) {
        val nextJob = UniffiInputStreamNextJobBox()
        val start = synchronized(lock) {
            when {
                cancelled || done -> UniffiInputStreamNextStart.DONE
                pendingJob != null -> UniffiInputStreamNextStart.CONCURRENT
                else -> {
                    ensureProducerStartedLocked()
                    pendingJob = nextJob
                    UniffiInputStreamNextStart.START
                }
            }
        }

        when (start) {
            UniffiInputStreamNextStart.DONE -> {
                uniffiCompleteInputStreamSuccess(
                    UniffiInputStreamNext.Done,
                    lowerNext,
                    callback,
                    callbackData,
                )
            }
            UniffiInputStreamNextStart.CONCURRENT -> {
                uniffiCompleteInputStreamUnexpected(
                    "UniFFI input stream received a concurrent next() request",
                    callback,
                    callbackData,
                )
            }
            UniffiInputStreamNextStart.START -> {
                val nextJobHandle = uniffiInputStreamNextJobHandleMap.insert(nextJob)
                droppedCallback.uniffiSetValue(
                    UniffiForeignFutureDroppedCallbackStruct(
                        nextJobHandle,
                        uniffiInputStreamNextDroppedCallbackImpl,
                    )
                )
                nextJob.set(
                    scope.launch {
                        runNext(callback, callbackData, nextJobHandle)
                    }
                )
            }
        }
    }

    override fun cancel() {
        val jobs = synchronized(lock) {
            if (cancelled) {
                return
            }
            cancelled = true
            done = true
            val currentPending = pendingJob
            val currentProducer = producerJob
            pendingJob = null
            producerJob = null
            currentPending to currentProducer
        }
        jobs.first?.cancel()
        jobs.second?.cancel()
        scopeJob.cancel()
        channel.close()
    }

    private fun ensureProducerStartedLocked() {
        if (producerJob != null) {
            return
        }
        producerJob = scope.launch {
            try {
                flow.collect { value ->
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
        nextJobHandle: Long,
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
        } catch (_: CancellationException) {
            finishNext(done = false)
        } catch (e: Throwable) {
            finishNext(done = true)
            uniffiCompleteInputStreamError(e, lowerError, callback, callbackData)
        } finally {
            try {
                uniffiInputStreamNextJobHandleMap.remove(nextJobHandle)
            } catch (_: InternalException) {
            }
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
            uniffiInputStreamHandleMap.remove(handle).cancel()
        } catch (_: InternalException) {
        }
    }
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
