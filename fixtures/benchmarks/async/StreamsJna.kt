import org.mozilla.uniffi.benchmarks.*
import kotlinx.coroutines.flow.*

fun finishBenchmarks() = benchShutdownWakeWorker()

suspend fun streamBenchmarks(backend: String, samples: Int, iterations: Int, warmup: Int, count: Int, filter: String) {
    for (pending in listOf(false, true)) {
        measure(if (pending) "stream_output_pending" else "stream_output_ready", backend, samples, iterations, warmup, filter, count) {
            var index = 0U
            benchOutputStream(count.toUInt(), pending).collect { value -> check(value == index); index++ }
            check(index == count.toUInt())
        }
    }
    fun input() = flow { repeat(count) { emit(it.toUInt()) } }
    measure("stream_input", backend, samples, iterations, warmup, filter, count) {
        check(benchInputStream(input()) == count.toULong() * (count - 1).toULong() / 2UL)
    }
    measure("stream_roundtrip", backend, samples, iterations, warmup, filter, count) {
        var index = 0U
        benchRoundtripStream(input()).collect { value -> check(value == index); index++ }
        check(index == count.toUInt())
    }
}
