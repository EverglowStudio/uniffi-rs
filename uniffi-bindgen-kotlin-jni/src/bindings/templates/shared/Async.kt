const val UNIFFI_RUST_FUTURE_CANCELLED = 1
const val UNIFFI_RUST_FUTURE_COMPLETE = 2

fun uniffiContinuationResume(continuation: kotlin.coroutines.Continuation<kotlin.Boolean>, ready: kotlin.Boolean) {
    continuation.resumeWith(Result.success(ready))
}

internal suspend fun <T> uniffiAwaitRustFuture(
    handle: Long,
    poll: (Long, kotlin.coroutines.Continuation<Boolean>) -> Unit,
    cancel: (Long) -> Unit,
    complete: (Long) -> T,
    free: (Long) -> Unit,
): T {
    val lock = Any()
    var live = true
    // A child Job observes parent cancellation immediately, while the caller
    // remains in a non-cancellable cleanup wait until core reports Ready.
    val cancellation = kotlinx.coroutines.Job(kotlinx.coroutines.currentCoroutineContext()[kotlinx.coroutines.Job])
    val registration = cancellation.invokeOnCompletion { cause ->
        if (cause != null) synchronized(lock) { if (live) cancel(handle) }
    }
    try {
        return kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) {
            while (!kotlin.coroutines.suspendCoroutine<Boolean> { poll(handle, it) }) {
                // Every core poll owns exactly one continuation, including cancellation.
            }
            synchronized(lock) {
                live = false
                complete(handle)
            }
        }
    } finally {
        registration.dispose()
        cancellation.complete()
        synchronized(lock) {
            live = false
            free(handle)
        }
    }
}

{%- for rust_result in root.rust_async_callable_results() %}
suspend fun {{ rust_result.async_await_future_fn() }}(rustFuture: kotlin.Long)
{%- if let Some(return_type) = rust_result.return_type %} : {{ return_type.type_kt }}{% endif %}
{
    {%- if rust_result.return_type.is_some() %}
    val completion = {{ rust_result.async_complete_class() }}()
    {%- endif %}
    return uniffiAwaitRustFuture(
        rustFuture,
        Scaffolding::{{ rust_result.async_poll_fn() }},
        Scaffolding::{{ rust_result.async_cancel_fn() }},
        { handle ->
            val code = Scaffolding.{{ rust_result.async_complete_fn() }}(handle
                {%- if rust_result.return_type.is_some() %}, completion{% endif %})
            if (code == UNIFFI_RUST_FUTURE_CANCELLED) throw kotlin.coroutines.cancellation.CancellationException()
            check(code == UNIFFI_RUST_FUTURE_COMPLETE) { "Invalid Rust future result: $code" }
            {%- if rust_result.return_type.is_some() %}
            completion.result!!.getOrThrow()
            {%- endif %}
        },
        Scaffolding::{{ rust_result.async_free_fn() }},
    )
}

{%- if let Some(return_type) = rust_result.return_type %}
class {{ rust_result.async_complete_class() }} {
    var result: Result<{{ return_type.type_kt }}>? = null
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
