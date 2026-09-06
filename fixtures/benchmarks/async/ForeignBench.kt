import org.mozilla.uniffi.benchmarks.*
import kotlinx.coroutines.*

class Callback(private val yielding: Boolean) : BenchAsyncCallback {
    override suspend fun call(value: UInt): UInt {
        if (yielding) yield()
        return value
    }
}

suspend fun measure(
    name: String, backend: String, samples: Int, iterations: Int, warmup: Int,
    filter: String, items: Int = 1, operation: suspend () -> Unit
) {
    if (filter != "all" && filter != name) return
    repeat(warmup) { operation() }
    repeat(samples) { sample ->
        val latencies = LongArray(iterations)
        val start = System.nanoTime()
        repeat(iterations) { index ->
            val callStart = System.nanoTime()
            operation()
            latencies[index] = System.nanoTime() - callStart
        }
        val elapsed = System.nanoTime() - start
        println("""{"kind":"sample","backend":"$backend","case":"$name","sample":$sample,"iterations":$iterations,"items_per_operation":$items,"elapsed_ns":$elapsed,"latency_ns":[${latencies.joinToString(",")}]}""")
    }
}

fun main(args: Array<String>) = runBlocking {
    val samples = args[0].toInt(); val iterations = args[1].toInt()
    val warmup = args[2].toInt(); val count = args[3].toInt()
    val filter = args[4]; val backend = args[5]
    val initStart = System.nanoTime()
    check(benchReady(42U) == 42U)
    println("""{"kind":"initialization","backend":"$backend","first_call_ns":${System.nanoTime() - initStart}}""")
    val ready = Callback(false); val yielding = Callback(true)
    measure("future_ready", backend, samples, iterations, warmup, filter) { check(benchReady(42U) == 42U) }
    measure("future_pending_wake", backend, samples, iterations, warmup, filter) { check(benchPending(43U) == 43U) }
    measure("callback_ready", backend, samples, iterations, warmup, filter) { check(benchCallback(ready, 44U) == 44U) }
    measure("callback_yield", backend, samples, iterations, warmup, filter) { check(benchCallback(yielding, 45U) == 45U) }
    if (backend != "jni") measure("future_cancel_lifecycle", backend, samples, iterations, warmup, filter) {
        val probe = BenchCancelProbe()
        try {
            val task = launch { benchCancel(probe); error("cancel returned a value") }
            withTimeout(10_000L) { while (!probe.isPolled()) yield() }
            task.cancelAndJoin()
            withTimeout(10_000L) { while (!probe.isDropped()) yield() }
        } finally { probe.close() }
    } else println("""{"kind":"unsupported","backend":"jni","case":"future_cancel_lifecycle","reason":"existing JNI suspendCoroutine does not propagate Job cancellation"}""")
    streamBenchmarks(backend, samples, iterations, warmup, count, filter)
    finishBenchmarks()
}
