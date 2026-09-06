// No Stream ABI exists in the JNI baseline. No replacement workload is measured.
external fun shutdownWakeWorker()
fun finishBenchmarks() = shutdownWakeWorker()

@Suppress("UNUSED_PARAMETER")
suspend fun streamBenchmarks(backend: String, samples: Int, iterations: Int, warmup: Int, count: Int, filter: String) { }
