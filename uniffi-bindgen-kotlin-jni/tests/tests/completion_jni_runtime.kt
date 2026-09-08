package uniffi

import kotlinx.coroutines.runBlocking
import uniffi.uniffi_bindgen_tests.asyncRoundtripString

fun main() = runBlocking {
    // Ordinary String completes through the real generated consuming JNI entry.
    check(asyncRoundtripString("completion ready") == "completion ready")
    val completion = CompleteRustFuture11()
    try {
        JniRuntimeProbe.completionLiftFailure(completion)
        error("native lift failure was swallowed")
    } catch (failure: InternalException) {
        check(failure.message == "Exception calling CompleteRustFuture11.complete: NULL")
    }
    val missing = runCatching { completion.getValue() }.exceptionOrNull()
    check(missing is IllegalStateException && missing.message == "Rust future completion was not published")
    println("completion JNI: real String terminal success, original lift exception translation, unpublished slot and natural exit passed")
}
