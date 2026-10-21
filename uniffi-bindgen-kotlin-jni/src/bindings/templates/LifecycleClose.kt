    override suspend fun {{ meth.callable.name_kt() }}() = kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) {
        var uniffiCloseHandle: kotlin.Long? = null
        val completion = synchronized(uniffiCloseLock) {
            uniffiCloseResult ?: kotlinx.coroutines.CompletableDeferred<Unit>().also {
                uniffiCloseHandle = uniffiCloneHandle()
                uniffiCloseResult = it
            }
        }
        uniffiCloseHandle?.let { uniffiCloseHandle ->
            val outcome = runCatching {
                try {
                    uniffi.{{ meth.callable.result.async_await_future_fn() }} {
                        uniffi.Scaffolding.{{ meth.jni_method_name }}(uniffiCloseHandle)
                    }
                } finally {
                    destroy()
                }
            }
            outcome.fold({ completion.complete(it) }, { completion.completeExceptionally(it) })
        }
        completion.await()
    }

    /** Run a scope and await resource cleanup, including on cancellation. */
    suspend fun <R> use(block: suspend ({{ cls.name_kt() }}) -> R): R {
        var failure: Throwable? = null
        try {
            return block(this)
        } catch (error: Throwable) {
            failure = error
            throw error
        } finally {
            try {
                {{ meth.callable.name_kt() }}()
            } catch (closeError: Throwable) {
                if (failure == null) throw closeError
                if (failure !== closeError) failure.addSuppressed(closeError)
            }
        }
    }
