package uniffi

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

fun main() = runBlocking {
    var polls = 0
    var releases = 0
    val stream = RustStream<Int?>(
        pull = { polls++; if (polls == 1) StreamStep.Item(null) else StreamStep.Done },
        release = { releases++ },
        discard = {},
    )
    check(stream.toList() == listOf<Int?>(null))
    check(polls == 2 && releases == 1)
    stream.close()
    check(releases == 1)
    try { stream.collect(); error("second collect accepted") } catch (_: IllegalStateException) {}

    val entered = CompletableDeferred<Unit>()
    val cleanup = CompletableDeferred<Unit>()
    val pending = RustStream<Int>(
        pull = { entered.complete(Unit); awaitCancellation() },
        release = { cleanup.await(); releases++ },
        discard = {},
    )
    val consuming = launch { pending.collect() }
    entered.await()
    consuming.cancel()
    yield()
    check(!consuming.isCompleted)
    consuming.cancel()
    cleanup.complete(Unit)
    consuming.join()
    check(releases == 2)

    var produced = 0
    val input = UniffiInputStream(flow {
        repeat(3) { produced++; emit(it) }
    })
    check(produced == 0)
    check(input.next() == StreamStep.Item(0))
    delay(20)
    check(produced == 1)
    check(input.next() == StreamStep.Item(1))
    input.close()
    check(produced == 2)
    input.close()
    var callbacks = 0
    var started = false
    val beforeStart = uniffiCreateCallback(101L, 101L, { callbacks++ }) {
        started = true
        { callbacks++ }
    }
    beforeStart.cancel()
    beforeStart.start()
    beforeStart.join()
    check(!started && callbacks == 1)

    val beforeStartGate = CompletableDeferred<Unit>()
    val beforeStartDone = CompletableDeferred<Unit>()
    var earlyCleanup = false
    val cancelledWithInputs = uniffiCreateCallback(105L, 105L,
        { check(earlyCleanup); callbacks++; beforeStartDone.complete(Unit) },
        cleanup = { beforeStartGate.await(); earlyCleanup = true },
    ) { error("cancelled callback body must not run") }
    cancelledWithInputs.cancel()
    cancelledWithInputs.start()
    cancelledWithInputs.join()
    check(!beforeStartDone.isCompleted)
    beforeStartGate.complete(Unit)
    beforeStartDone.await()
    check(callbacks == 2)

    val callbackStarted = CompletableDeferred<Unit>()
    val callbackCleanup = CompletableDeferred<Unit>()
    val callbackFinished = CompletableDeferred<Unit>()
    val callback = uniffiCreateCallback(103L, 103L, { callbacks++; callbackFinished.complete(Unit) }) {
        try {
            callbackStarted.complete(Unit)
            awaitCancellation()
        } finally {
            withContext(NonCancellable) { callbackCleanup.await() }
        }
    }
    callback.start()
    callbackStarted.await()
    callback.cancel()
    yield()
    check(!callbackFinished.isCompleted)
    callback.cancel()
    callbackCleanup.complete(Unit)
    callback.join()
    callbackFinished.await()
    check(callbacks == 3)

    val polled = CompletableDeferred<Unit>()
    val cancelled = CompletableDeferred<Unit>()
    val continuation = java.util.concurrent.atomic.AtomicReference<kotlin.coroutines.Continuation<Boolean>>()
    var freed = false
    val waiting = launch {
        uniffiAwaitRustFuture(
            2L,
            { _, cont -> continuation.set(cont); polled.complete(Unit) },
            { _ -> cancelled.complete(Unit) },
            { _ -> throw CancellationException() },
            { _ -> freed = true },
        )
    }
    polled.await()
    waiting.cancel()
    cancelled.await()
    check(!waiting.isCompleted && !freed)
    waiting.cancel()
    continuation.get().resumeWith(Result.success(true))
    waiting.join()
    check(freed)
    println("jni Kotlin runtime: 8 scenarios passed")
}
