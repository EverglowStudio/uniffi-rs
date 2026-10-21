    // The first caller performs cleanup in NonCancellable; other callers join
    // its result. No object lock is held across suspension.
    override suspend fun {{ meth.name()|fn_name }}() = kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) {
        var uniffiCloseHandle: Long? = null
        val completion = synchronized(uniffiCloseLock) {
            uniffiCloseResult ?: kotlinx.coroutines.CompletableDeferred<Unit>().also {
                uniffiCloseHandle = uniffiCloneHandle()
                uniffiCloseResult = it
            }
        }
        uniffiCloseHandle?.let { uniffiCloseHandle ->
            val outcome = runCatching {
                try {
                    {% call kt::call_async(meth, true) %}{% endcall %}
                } finally {
                    destroy()
                }
            }
            outcome.fold({ completion.complete(it) }, { completion.completeExceptionally(it) })
        }
        completion.await()
    }

    /** Run a scope and await resource cleanup, including on cancellation. */
    suspend fun <R> use(block: suspend ({{ impl_class_name }}) -> R): R {
        var failure: Throwable? = null
        try {
            return block(this)
        } catch (error: Throwable) {
            failure = error
            throw error
        } finally {
            try {
                {{ meth.name()|fn_name }}()
            } catch (closeError: Throwable) {
                if (failure == null) throw closeError
                if (failure !== closeError) failure.addSuppressed(closeError)
            }
        }
    }
