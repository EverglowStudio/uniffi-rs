// Test-only native terminal adapter. Never emitted into product bindings.
internal suspend fun<T, F, E: kotlin.Exception> uniffiRustCallAsyncMock(
    rustFuture: Long,
    pollFunc: (Long, UniffiRustFutureContinuationCallback, Long) -> Unit,
    cancelFunc: (Long) -> Unit,
    completeFunc: (Long, UniffiRustCallStatus) -> F,
    freeFunc: (Long) -> Unit,
    liftFunc: (F) -> T,
    errorHandler: UniffiRustCallStatusErrorHandler<E>,
    discardResult: suspend (T) -> Unit = {},
): T = uniffiRustCallAsync(rustFuture, pollFunc, cancelFunc, { handle, status ->
    val result = runCatching { completeFunc(handle, status) }
    try { freeFunc(handle) } catch (release: Throwable) {
        val failure = result.exceptionOrNull()
        if (failure == null) throw release
        failure.addSuppressed(release)
    }
    result.getOrThrow()
}, freeFunc, liftFunc, errorHandler, discardResult)
