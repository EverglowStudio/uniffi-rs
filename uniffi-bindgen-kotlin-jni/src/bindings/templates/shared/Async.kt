const val UNIFFI_RUST_FUTURE_CANCELLED = 1
const val UNIFFI_RUST_FUTURE_COMPLETE = 2

// Initialization is lazy, after Scaffolding has finished loading the library.
// No class initializer waits for a thread that must enter that same class.
private object UniffiFutureReceiver {
    private val notifications = java.util.concurrent.ConcurrentHashMap<Long, kotlin.coroutines.Continuation<Boolean>>()
    private val nextId = java.util.concurrent.atomic.AtomicLong(0)
    private val failure = java.util.concurrent.atomic.AtomicReference<Throwable>()
    private val started by lazy {
        runCatching {
            check(Scaffolding.futureNotificationTake(false) == 0L)
            val workers = java.util.concurrent.ThreadPoolExecutor(
                0, Int.MAX_VALUE, 60L, java.util.concurrent.TimeUnit.SECONDS,
                java.util.concurrent.SynchronousQueue<Runnable>(),
                java.util.concurrent.ThreadFactory { task ->
                    Thread(task, "uniffi-future-resume").apply { isDaemon = true }
                },
            )
            val receiverReady = java.util.concurrent.CountDownLatch(1)
            val leader = object : Runnable {
                override fun run() {
                    val message = try {
                        val message = Scaffolding.futureNotificationTake(true)
                        check(message != 0L && message != Long.MIN_VALUE)
                        // SynchronousQueue hands off to another worker (or starts
                        // one). Transfer the sole take role before user code can
                        // block this thread in an Unconfined nested call.
                        workers.execute(this)
                        message
                    } catch (error: Throwable) {
                        // No handoff, no delivery: leave the registration intact.
                        // Existing admitted calls are not given fake cleanup.
                        failure.set(error)
                        throw error
                    }
                    // Handoff succeeded. Resume errors belong to this worker,
                    // not receiver admission, and must never redeliver a message.
                    val id = if (message < 0) -message else message
                    val continuation = checkNotNull(notifications.remove(id)) {
                        "Unknown Rust future notification: $id"
                    }
                    continuation.resumeWith(Result.success(message < 0))
                    // Return to the pool; only the successor may take again.
                }
            }
            workers.execute {
                try {
                    check(Scaffolding.futureNotificationTake(false) == 0L)
                    // Confirm an independent worker while the initial leader
                    // still occupies this one. Neither task reads lazy started.
                    val followerReady = java.util.concurrent.CountDownLatch(1)
                    workers.execute { followerReady.countDown() }
                    followerReady.await()
                } catch (error: Throwable) {
                    failure.set(error)
                    throw error
                } finally { receiverReady.countDown() }
                leader.run()
            }
            receiverReady.await()
            failure.get()?.let { throw it }
        }
    }

    fun ensureReady() {
        started.getOrThrow()
        failure.get()?.let { throw it }
    }

    fun poll(handle: Long, continuation: kotlin.coroutines.Continuation<Boolean>, nativePoll: (Long, Long) -> Boolean): Boolean {
        val id = nextId.updateAndGet { current ->
            check(current < Long.MAX_VALUE) { "Rust future notification IDs exhausted" }
            current + 1
        }
        check(notifications.putIfAbsent(id, continuation) == null)
        try {
            val ready = nativePoll(handle, id)
            if (ready) check(notifications.remove(id) === continuation)
            return ready
        } catch (error: Throwable) {
            // Native poll failures occur before Scheduler ownership transfer.
            notifications.remove(id)
            throw error
        }
    }
}

internal fun uniffiPollRustFuture(
    handle: Long, continuation: kotlin.coroutines.Continuation<Boolean>, poll: (Long, Long) -> Boolean,
): Boolean = UniffiFutureReceiver.poll(handle, continuation, poll)

// Keep one cancellation registration for the whole Rust call. Each poll keeps
// its original atomic continuation until Rust sends Wake, Ready, or failure.
@OptIn(kotlinx.coroutines.InternalCoroutinesApi::class)
internal suspend fun <T> uniffiAwaitRustFuture(
    future: () -> Long,
    poll: (Long, kotlin.coroutines.Continuation<Boolean>) -> Boolean,
    cancel: (Long) -> Unit,
    completeAndFree: (Long) -> T,
    free: (Long) -> Unit,
): T {
    UniffiFutureReceiver.ensureReady()
    val handle = future()
    val callerContext = kotlin.coroutines.coroutineContext
    val lock = Any()
    var live = true
    var cancellation: kotlinx.coroutines.DisposableHandle? = null
    try {
        // Immediate callbacks include already-cancelled and normally completed
        // Jobs (null cause). Registration follows the factory, before any poll.
        cancellation = callerContext[kotlinx.coroutines.Job]?.invokeOnCompletion(
            onCancelling = true,
            invokeImmediately = true,
        ) {
            try {
                synchronized(lock) { if (live) cancel(handle) }
            } catch (error: Throwable) {
                kotlinx.coroutines.handleCoroutineException(
                    callerContext,
                    kotlinx.coroutines.CompletionHandlerException(
                        "Exception in Rust future cancellation handler", error,
                    ),
                )
            }
        }
        do {
            val ready = kotlin.coroutines.suspendCoroutine<Boolean> { continuation ->
                val polledReady = try {
                    poll(handle, continuation)
                } catch (error: Throwable) {
                    // A failed native poll did not transfer its callback.
                    continuation.resumeWith(Result.failure(error))
                    return@suspendCoroutine
                }
                if (polledReady) continuation.resumeWith(Result.success(true))
            }
        } while (!ready)
        // Wait for admitted native cancel to finish, then close its entry.
        // Complete outside the lock, in the caller context, without suspension.
        synchronized(lock) { live = false }
        return completeAndFree(handle)
    } finally {
        // Disposal is not a barrier for an already-running handler. Only this
        // coroutine writes live; cancellation reads it under the same lock.
        try {
            cancellation?.dispose()
        } finally {
            // Generated completeAndFree owns release, including native errors.
            // Only failures before terminal admission use fallback free here.
            if (live) {
                synchronized(lock) { live = false }
                free(handle)
            }
        }
    }
}

{%- for rust_result in root.rust_async_callable_results() %}
suspend fun {{ rust_result.async_await_future_fn() }}(rustFuture: () -> kotlin.Long)
{%- if let Some(return_type) = rust_result.return_type %} : {{ return_type.type_kt }}{% endif %}
{
    {%- if rust_result.return_type.is_some() %}
    val completion = {{ rust_result.async_complete_class() }}()
    {%- endif %}
    return uniffiAwaitRustFuture(
        rustFuture,
        { handle, continuation -> uniffiPollRustFuture(handle, continuation, Scaffolding::{{ rust_result.async_poll_fn() }}) },
        Scaffolding::{{ rust_result.async_cancel_fn() }},
        { handle ->
            val code = Scaffolding.{{ rust_result.async_complete_fn() }}(handle
                {%- if rust_result.return_type.is_some() %}, completion{% endif %})
            if (code == UNIFFI_RUST_FUTURE_CANCELLED) throw kotlin.coroutines.cancellation.CancellationException()
            check(code == UNIFFI_RUST_FUTURE_COMPLETE) { "Invalid Rust future result: $code" }
            {%- if rust_result.return_type.is_some() %}
            completion.getValue()
            {%- endif %}
        },
        Scaffolding::{{ rust_result.async_free_fn() }},
    )
}

{%- if let Some(return_type) = rust_result.return_type %}
class {{ rust_result.async_complete_class() }} {
    private var result: Result<{{ return_type.type_kt }}>? = null

    fun getValue(): {{ return_type.type_kt }} {
        return checkNotNull(result) { "Rust future completion was not published" }.getOrThrow()
    }

    fun complete(
        {%- for ffi_type in return_type.ffi_types %}
        v{{loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
    ) {
        this.result = Result.success({{ return_type.lift_fn_kt() }}(
            {%- for _ in return_type.ffi_types %}
            v{{loop.index0 }},
            {%- endfor %}
        ))
    }
}
{%- endif %}
{%- endfor %}

private val uniffiCallbackJobs = java.util.concurrent.ConcurrentHashMap<Long, kotlinx.coroutines.Job>()

internal fun uniffiCreateCallback(
    key: Long,
    released: Long,
    failed: () -> Unit,
    cleanup: (suspend () -> Unit)? = null,
    block: suspend () -> (() -> Unit),
): kotlinx.coroutines.Job {
    val owner = kotlinx.coroutines.SupervisorJob()
    val delivery = java.util.concurrent.atomic.AtomicReference(failed)
    val job = kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Default + owner).launch(
        start = kotlinx.coroutines.CoroutineStart.LAZY,
    ) {
        try { delivery.set(block()) }
        catch (_: Throwable) { /* The default delivery reports cancellation/failure. */ }
    }
    check(uniffiCallbackJobs.putIfAbsent(key, job) == null)
    fun finish() {
        uniffiCallbackJobs.remove(key, job)
        owner.complete()
        try {
            val send = delivery.getAndSet({})
            try { send() } catch (_: Throwable) { failed() }
        } finally { Scaffolding.kotlinCallbackReleased(released) }
    }
    job.invokeOnCompletion {
        if (cleanup == null) { finish() }
        else {
            // This Job is owned by the same callback owner. Even a callback
            // cancelled before first dispatch must close eagerly lifted inputs.
            val releasing = kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.Default + owner).launch {
                try { kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) { cleanup() } }
                catch (_: Throwable) { delivery.set(failed) }
            }
            releasing.invokeOnCompletion { finish() }
        }
    }

    return job
}

internal fun uniffiLaunchCallback(key: Long, released: Long, failed: () -> Unit, cleanup: (suspend () -> Unit)? = null, block: suspend () -> (() -> Unit)) {
    uniffiCreateCallback(key, released, failed, cleanup, block).start()
}

fun uniffiCancelCallback(key: Long) {
    uniffiCallbackJobs[key]?.cancel()
}
