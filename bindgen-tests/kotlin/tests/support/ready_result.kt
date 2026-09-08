import kotlinx.coroutines.*
import kotlin.test.*
import uniffi.uniffi_bindgen_tests.*

// Compiled with generated sources in the same Kotlin module. Both interleavings
// use a real native Ready and the original production driver; only delivery of
// that callback / Job.cancel is controlled. This is an internal native driver
// regression, not a claim that an uninstrumented public wrapper fixes the race.
fun main() = runBlocking {
    for (queued in listOf(false, true)) {
        withTimeout(10_000) {
            val body = UnifiedGate()
            val cleanup = UnifiedGate()
            val tracker = UnifiedTracker()
            val readyDelivery = CompletableDeferred<() -> Unit>()
            body.open()
            var nativeReady = false
            var completes = 0
            var frees = 0
            var cancelledOutcome = false
            var delivered: Long? = null
            val call = launch(start = CoroutineStart.UNDISPATCHED) {
                val ownJob = currentCoroutineContext().job
                try {
                    delivered = uniffiRustCallAsync(
                        UniffiLib.uniffi_uniffi_bindgen_tests_fn_func_unified_values_factory(
                            1, 0, FfiConverterTypeUnifiedGate.lower(body),
                            FfiConverterTypeUnifiedTracker.lower(tracker), FfiConverterTypeUnifiedGate.lower(cleanup)),
                        { future, callback, data ->
                            UniffiLib.ffi_uniffi_bindgen_tests_rust_future_poll_u64(future,
                                object: UniffiRustFutureContinuationCallback {
                                    override fun callback(data: Long, pollResult: Byte) {
                                        if (pollResult == UNIFFI_RUST_FUTURE_POLL_READY) {
                                            nativeReady = true
                                            if (queued) {
                                                check(readyDelivery.complete { callback.callback(data, pollResult) })
                                            } else {
                                                callback.callback(data, pollResult)
                                                ownJob.cancel()
                                            }
                                        } else {
                                            callback.callback(data, pollResult)
                                        }
                                    }
                                }, data)
                        },
                        { future -> UniffiLib.ffi_uniffi_bindgen_tests_rust_future_cancel_u64(future) },
                        { future, status ->
                            completes++
                            // Count completed protocol consumption, not native internal destructors.
                            UniffiLib.ffi_uniffi_bindgen_tests_rust_future_complete_and_free_u64(future, status).also { frees++ }
                        },
                        { future -> frees++; UniffiLib.ffi_uniffi_bindgen_tests_rust_future_free_u64(future) },
                        { handle -> handle },
                        UnifiedException.ErrorHandler,
                    )
                } catch (_: CancellationException) { cancelledOutcome = true }
            }
            suspend fun closeDelivered(handle: Long) = withContext(NonCancellable) {
                uniffiRustCallAsync(
                    UniffiLib.uniffi_uniffi_bindgen_tests_fn_func_unified_values_factory_stream_close(handle),
                    { future, callback, data -> UniffiLib.ffi_uniffi_bindgen_tests_rust_future_poll_void(future, callback, data) },
                    { future -> UniffiLib.ffi_uniffi_bindgen_tests_rust_future_cancel_void(future) },
                    { future, status -> UniffiLib.ffi_uniffi_bindgen_tests_rust_future_complete_and_free_void(future, status) },
                    { future -> UniffiLib.ffi_uniffi_bindgen_tests_rust_future_free_void(future) },
                    {}, UniffiNullRustCallStatusErrorHandler,
                )
            }
            var close: Job? = null
            try {
                if (queued) {
                    readyDelivery.await().invoke()
                    // The Ready waiter was queued on this event loop. Cancel it
                    // before it resumes, exercising await's prompt cancellation.
                    call.cancel()
                }
                call.join()
                assertTrue(nativeReady)
                assertEquals(1, completes)
                assertEquals(1, frees)
                assertFalse(cancelledOutcome, "Ready success must be retained")
                val handle = assertNotNull(delivered)
                assertEquals(0u, tracker.counts().dropped, "the caller owns the delivered subscription")
                close = launch { closeDelivered(handle) }
                withTimeout(5_000) { while (tracker.counts().dropped != 1u) yield() }
                assertFalse(close.isCompleted)
                assertEquals(0u, tracker.counts().cleaned)
                close.cancel()
                assertFalse(close.isCompleted)
                cleanup.open()
                close.join()
                assertEquals(1u, tracker.counts().cleaned)
                println("PASS native Ready preserves result and caller closes: queued=$queued")
            } finally {
                cleanup.open()
                if (close != null) close.join() else delivered?.let { closeDelivered(it) }
                body.destroy(); cleanup.destroy(); tracker.destroy()
            }
        }
    }
    println("2 real Kotlin JNA Ready interleavings passed")
}
