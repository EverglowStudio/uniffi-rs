package uniffi

import kotlinx.coroutines.*
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.ConcurrentHashMap

// The probe deliberately keeps separate native controls for observing cleanup.
// Compose them into the same terminal-owner contract as generated completions.
private suspend fun <T> awaitReceiverFuture(
    future: () -> Long,
    poll: (Long, kotlin.coroutines.Continuation<Boolean>) -> Boolean,
    cancel: (Long) -> Unit,
    complete: (Long) -> T,
    free: (Long) -> Unit,
): T = uniffiAwaitRustFuture(future, poll, cancel,
    { handle -> try { complete(handle) } finally { free(handle) } }, free)

private suspend fun receiverCall(
    mode: Int,
    pollObserved: (Long) -> Unit = {},
    freed: () -> Unit = {},
): Int = awaitReceiverFuture(
    { JniRuntimeProbe.receiverNew(mode) },
    { handle, continuation ->
        val ready = uniffiPollRustFuture(handle, continuation, JniRuntimeProbe::receiverPoll)
        pollObserved(handle)
        ready
    },
    JniRuntimeProbe::receiverCancel,
    { handle ->
        val value = JniRuntimeProbe.receiverComplete(handle)
        if (value == -1) throw CancellationException()
        value
    },
    { handle -> JniRuntimeProbe.receiverFree(handle); freed() },
)

private suspend fun eventually(check: () -> Boolean) = withTimeout(10_000) {
    while (!check()) delay(1)
}

fun main() = runBlocking {
    // First async admission cold-starts the actual generated receiver. No
    // preceding Scaffolding call can accidentally hide a class-init deadlock.
    var readyPolls = 0
    var readyFrees = 0
    check(receiverCall(1, { readyPolls++ }, { readyFrees++ }) == 47)
    check(readyPolls == 1 && readyFrees == 1)
    var earlyPolls = 0
    check(receiverCall(2, { earlyPolls++ }) == 47)
    check(earlyPolls == 2)
    check(JniRuntimeProbe.checkAttachment()) // Preserve the original assertions.

    // Use the real generated completion and error translator. No replacement
    // terminal lambda can hide a missing conversion/error path in scaffolding.
    try {
        awaitRustFuture5 { JniRuntimeProbe.conversionFailureNew() }
        error("native conversion failure was lost")
    } catch (error: InternalException) {
        check(error.message == "opt6 native conversion failure")
    }


    // Two nested synchronous runBlocking calls from Unconfined resumptions.
    // Every blocked resumer must already have handed the take role to a successor.
    val pending = LinkedBlockingQueue<Long>()
    val workers = ConcurrentHashMap.newKeySet<Long>()
    val freed = AtomicInteger()
    suspend fun nested(depth: Int) {
        val published = java.util.concurrent.atomic.AtomicBoolean()
        check(receiverCall(0, { handle ->
            if (published.compareAndSet(false, true)) pending.put(handle)
        }, { freed.incrementAndGet() }) == 47)
        val thread = Thread.currentThread()
        check(thread.isDaemon && thread.name.substringBefore(" @") == "uniffi-future-resume") { "resume thread: ${thread.name}, daemon=${thread.isDaemon}" }
        workers.add(thread.threadId())
        if (depth > 0) runBlocking(Dispatchers.Unconfined) { nested(depth - 1) }
    }
    val nesting = launch(Dispatchers.Unconfined) { nested(2) }
    repeat(3) {
        val handle = checkNotNull(pending.poll(10, TimeUnit.SECONDS)) { "successor blocked during nested resume" }
        check(JniRuntimeProbe.receiverWake(handle, 2))
        check(JniRuntimeProbe.receiverWake(handle, 1))
    }
    withTimeout(10_000) { nesting.join() }
    check(workers.size == 3 && freed.get() == 3)

    // Real native cancellation with original outstanding notification retained,
    // then multiple cleanup Pending wakes. Caller ThreadContext remains intact
    // for poll, completion and free, including NonCancellable drain.
    val local = ThreadLocal<String>()
    val executor = java.util.concurrent.Executors.newSingleThreadExecutor()
    val dispatcher = executor.asCoroutineDispatcher()
    try {
        withContext(dispatcher + local.asContextElement("receiver-caller")) {
            val handle = AtomicLong()
            val polls = AtomicInteger()
            var frees = 0
            val waiting = launch(start = CoroutineStart.UNDISPATCHED) {
                try {
                    awaitReceiverFuture<Unit>(
                        { check(local.get() == "receiver-caller"); JniRuntimeProbe.receiverNew(0) },
                        { current, continuation ->
                            check(local.get() == "receiver-caller")
                            val ready = uniffiPollRustFuture(current, continuation, JniRuntimeProbe::receiverPoll)
                            handle.set(current); polls.incrementAndGet(); ready
                        },
                        JniRuntimeProbe::receiverCancel,
                        { current ->
                            check(local.get() == "receiver-caller")
                            check(JniRuntimeProbe.receiverComplete(current) == -1)
                            throw CancellationException()
                        },
                        { current ->
                            check(local.get() == "receiver-caller")
                            JniRuntimeProbe.receiverFree(current); frees++
                        },
                    )
                    error("cancelled Run returned success")
                } catch (_: CancellationException) {}
            }
            eventually { handle.get() != 0L }
            waiting.cancel()
            eventually { JniRuntimeProbe.receiverDropped(handle.get()) && polls.get() >= 2 }
            check(!waiting.isCompleted && frees == 0)
            repeat(2) {
                val before = polls.get()
                check(JniRuntimeProbe.receiverWake(handle.get(), 0))
                eventually { polls.get() > before }
                waiting.cancel()
                check(!waiting.isCompleted && frees == 0)
            }
            check(JniRuntimeProbe.receiverWake(handle.get(), 2))
            withTimeout(10_000) { waiting.join() }
            check(frees == 1)
        }
    } finally { dispatcher.close() }
    check(local.get() == null)
    val receiverThreads = Thread.getAllStackTraces().keys.filter { it.name.startsWith("uniffi-future-") }
    // After the three blocked resumers unwind, the chain must still serve a
    // fresh queued early wake. A thread name/count is not proof of a live leader.
    var followupPolls = 0
    check(receiverCall(2, { followupPolls++ }) == 47 && followupPolls == 2)
    check(receiverThreads.none { it.name == "uniffi-future-pump" })
    check(receiverThreads.all { it.isDaemon })
    println("JNI receiver: cold admission, Ready, early wake, detached native wake, two-level blocking reentry, successor progress, context and cancellation drain passed; natural exit")
}
