package uniffi

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlin.coroutines.startCoroutine

// The generated terminal entry owns complete and free together. Model that
// contract explicitly; the fallback free is used only before terminal admission.
private suspend fun <T> awaitMockRustFuture(
    future: () -> Long,
    poll: (Long, kotlin.coroutines.Continuation<Boolean>) -> Boolean,
    cancel: (Long) -> Unit,
    complete: (Long) -> T,
    free: (Long) -> Unit,
): T = uniffiAwaitRustFuture(future, poll, cancel,
    { handle -> try { complete(handle) } finally { free(handle) } }, free)

fun main(args: Array<String>) = runBlocking {
    if (args.singleOrNull() == "init-failure") {
        var admitted = false
        try {
            awaitMockRustFuture<Unit>(
                { admitted = true; 1L }, { _, _ -> error("polled after init failure") },
                { _ -> }, { _ -> }, { _ -> error("freed an uncreated future") },
            )
            error("receiver failure was swallowed")
        } catch (error: IllegalStateException) {
            check(error.message == "receiver init failed" && !admitted)
        }
        println("receiver initialization failure precedes factory: passed")
        return@runBlocking
    }
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

    var readyPolls = 0
    var readyCompletes = 0
    var readyFrees = 0
    val readyValue = awaitMockRustFuture<Int?>(
        { 3L },
        { _, _ -> readyPolls++; true },
        { _ -> error("ready call unexpectedly cancelled") },
        { _ -> readyCompletes++; null },
        { _ -> readyFrees++ },
    )
    check(readyValue == null && readyPolls == 1 && readyCompletes == 1 && readyFrees == 1)

    var earlyWakePolls = 0
    var earlyWakeFrees = 0
    val earlyWakeValue = awaitMockRustFuture(
        { 4L },
        { _, cont ->
            if (++earlyWakePolls == 1) {
                cont.resumeWith(Result.success(false))
                false
            } else true
        },
        { _ -> error("early wake call unexpectedly cancelled") },
        { _ -> 42 },
        { _ -> earlyWakeFrees++ },
    )
    check(earlyWakeValue == 42 && earlyWakePolls == 2 && earlyWakeFrees == 1)

    val polled = CompletableDeferred<Unit>()
    val cancelled = CompletableDeferred<Unit>()
    val continuation = java.util.concurrent.atomic.AtomicReference<kotlin.coroutines.Continuation<Boolean>>()
    val cleanupReady = java.util.concurrent.atomic.AtomicBoolean(false)
    var freed = false
    val waiting = launch {
        awaitMockRustFuture(
            { 2L },
            { _, cont ->
                if (cleanupReady.get()) true
                else { continuation.set(cont); polled.complete(Unit); false }
            },
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
    cleanupReady.set(true)
    continuation.get().resumeWith(Result.success(false))
    waiting.join()
    check(freed)
    checkAwaitContext()
    checkAwaitCancellationDrain()
    checkTerminalRelease()
    checkNotificationPublication()
    checkSingleRegistration()
    checkJobBoundaries()
    checkCancellationReporting()
    checkCancelledPollFailure()
    checkLiveCancellationGate()
    println("jni Kotlin runtime: original 21 + 12 terminal release scenarios passed")
}

private suspend fun checkAwaitContext() = coroutineScope {
    // Already-cancelled callers still own a Rust handle: cancel before its first
    // poll, consume the terminal cancellation, and free it exactly once.
    var preCancelledPolls = 0
    var preCancelledFrees = 0
    launch(start = CoroutineStart.UNDISPATCHED) {
        currentCoroutineContext()[Job]!!.cancel()
        var cancelled = false
        try {
            awaitMockRustFuture<Unit>(
                { 11L },
                { _, _ -> check(cancelled); preCancelledPolls++; true },
                { _ -> cancelled = true },
                { _ -> throw CancellationException() },
                { _ -> preCancelledFrees++ },
            )
            error("pre-cancellation was lost")
        } catch (_: CancellationException) {}
    }.join()
    check(preCancelledPolls == 1 && preCancelledFrees == 1)

    var inPollFrees = 0
    launch(start = CoroutineStart.UNDISPATCHED) {
        val caller = currentCoroutineContext()[Job]!!
        var cancelled = false
        try {
            awaitMockRustFuture<Unit>(
                { 12L },
                { _, _ -> caller.cancel(); check(cancelled); true },
                { _ -> cancelled = true },
                { _ -> throw CancellationException() },
                { _ -> inPollFrees++ },
            )
            error("cancellation during first poll was lost")
        } catch (_: CancellationException) {}
    }.join()
    check(inPollFrees == 1)

    // Rust may have committed a value while cleanup still waits. A late cancel
    // must not prevent the original continuation from draining and delivering it.
    val committed = CompletableDeferred<kotlin.coroutines.Continuation<Boolean>>()
    var cleanupReady = false
    var lateCancels = 0
    var lateCompletes = 0
    var lateFrees = 0
    var delivered = false
    val late = launch(start = CoroutineStart.UNDISPATCHED) {
        val value = awaitMockRustFuture(
            { 13L },
            { _, cont -> if (cleanupReady) true else { committed.complete(cont); false } },
            { _ -> lateCancels++ },
            { _ -> check(cleanupReady); lateCompletes++; 42 },
            { _ -> lateFrees++ },
        )
        check(value == 42)
        delivered = true
    }
    val continuation = committed.await()
    late.cancel()
    check(lateCancels == 1 && !late.isCompleted && lateFrees == 0)
    cleanupReady = true
    continuation.resumeWith(Result.success(false))
    late.join()
    check(delivered && lateCompletes == 1 && lateFrees == 1)

    var failureFrees = 0
    val failure = IllegalStateException("completion failed")
    try {
        awaitMockRustFuture<Unit>(
            { 14L }, { _, _ -> true }, { _ -> error("unexpected cancel") },
            { _ -> throw failure }, { _ -> failureFrees++ },
        )
        error("completion failure was swallowed")
    } catch (caught: IllegalStateException) {
        // Coroutine stack-trace recovery may copy an exception across a scope.
        check(caught.message == failure.message)
    }
    check(failureFrees == 1)

    // Use actual coroutine/dispatcher interception, not a direct custom
    // continuation. Check both restoration on the producer and the caller Job.
    val local = ThreadLocal<String?>()
    val executor = java.util.concurrent.Executors.newSingleThreadExecutor()
    val dispatcher = executor.asCoroutineDispatcher()
    try {
        withContext(dispatcher + local.asContextElement("caller")) {
            val callerContext = currentCoroutineContext()
            val callerThread = Thread.currentThread()
            var readyFrees = 0
            awaitMockRustFuture(
                { 15L }, { _, cont ->
                    check(cont.context[Job] === callerContext[Job])
                    check(local.get() == "caller" && Thread.currentThread() === callerThread)
                    true
                }, { _ -> error("unexpected cancel") }, { _ -> 7 }, { _ -> readyFrees++ },
            ).also { check(it == 7 && readyFrees == 1) }

            val pending = CompletableDeferred<kotlin.coroutines.Continuation<Boolean>>()
            val finished = java.util.concurrent.atomic.AtomicBoolean(false)
            val restored = java.util.concurrent.atomic.AtomicBoolean(false)
            var pendingFrees = 0
            val producer = launch(Dispatchers.Default + local.asContextElement("producer")) {
                val cont = pending.await()
                // The single caller thread can process this only after the
                // first poll has returned Pending and actually suspended.
                withContext(dispatcher) { check(Thread.currentThread() === callerThread) }
                check(local.get() == "producer")
                finished.set(true)
                cont.resumeWith(Result.success(false))
                check(local.get() == "producer")
                restored.set(true)
            }
            val value = awaitMockRustFuture(
                { 16L },
                { _, cont ->
                    check(cont.context[Job] === callerContext[Job])
                    check(local.get() == "caller" && Thread.currentThread() === callerThread)
                    if (finished.get()) true else { pending.complete(cont); false }
                },
                { _ -> error("unexpected cancel") },
                { _ -> check(local.get() == "caller" && Thread.currentThread() === callerThread); 8 },
                { _ -> check(local.get() == "caller"); pendingFrees++ },
            )
            producer.join()
            check(value == 8 && restored.get() && pendingFrees == 1)
        }
    } finally { dispatcher.close() }
    check(local.get() == null)
}

private suspend fun checkAwaitCancellationDrain() = coroutineScope {
    // Cancel the waiter while Rust still owns its first notification. Repeated
    // cancellation must neither replace that notification nor skip cleanup polls.
    val notifications = kotlinx.coroutines.channels.Channel<kotlin.coroutines.Continuation<Boolean>>(
        kotlinx.coroutines.channels.Channel.UNLIMITED,
    )
    var polls = 0
    var cancels = 0
    var completes = 0
    var frees = 0
    var cleanupReady = false
    val waiting = launch(start = CoroutineStart.UNDISPATCHED) {
        try {
            awaitMockRustFuture<Unit>(
                { 17L },
                { _, cont ->
                    polls++
                    if (cleanupReady) true
                    else { check(notifications.trySend(cont).isSuccess); false }
                },
                { _ -> cancels++ },
                { _ -> check(cleanupReady); completes++; throw CancellationException() },
                { _ -> check(cleanupReady); frees++ },
            )
            error("Rust cancellation was lost")
        } catch (_: CancellationException) {}
    }
    val original = notifications.receive()
    waiting.cancel()
    yield()
    check(cancels == 1 && polls == 1 && completes == 0 && frees == 0 && !waiting.isCompleted)
    // Cancellation leaves this original notification gate pending.
    original.resumeWith(Result.success(false))
    val cleanupPoll1 = notifications.receive()
    waiting.cancel()
    yield()
    check(cancels == 1 && polls == 2 && completes == 0 && frees == 0 && !waiting.isCompleted)
    cleanupPoll1.resumeWith(Result.success(false))
    val cleanupPoll2 = notifications.receive()
    yield()
    check(polls == 3 && completes == 0 && frees == 0 && !waiting.isCompleted)
    cleanupReady = true
    cleanupPoll2.resumeWith(Result.success(false))
    waiting.join()
    check(polls == 4 && cancels == 1 && completes == 1 && frees == 1)
    notifications.close()

    // Ready is notified while the caller dispatcher is occupied by this body.
    // Cancel before it can dispatch: Rust's terminal notification must survive
    // and its committed value must be lifted.
    val notified = CompletableDeferred<kotlin.coroutines.Continuation<Boolean>>()
    var latePolls = 0
    var lateCompletes = 0
    var lateFrees = 0
    var delivered = false
    val late = launch(start = CoroutineStart.UNDISPATCHED) {
        val value = awaitMockRustFuture(
            { 18L },
            { _, cont -> latePolls++; notified.complete(cont); false },
            { _ -> },
            { _ -> lateCompletes++; 91 },
            { _ -> lateFrees++ },
        )
        check(value == 91)
        delivered = true
    }
    notified.await().resumeWith(Result.success(true))
    late.cancel()
    late.join()
    check(delivered && latePolls == 1 && lateCompletes == 1 && lateFrees == 1)

    // Cancellation observation must no longer allocate/attach a child Job.
    launch(start = CoroutineStart.UNDISPATCHED) {
        val caller = currentCoroutineContext()[Job]!!
        awaitMockRustFuture(
            { 19L },
            { _, cont ->
                check(cont.context[Job] === caller)
                check(caller.children.none()) { "await registered a child Job" }
                true
            },
            { _ -> error("unexpected cancellation") },
            { _ -> 19 },
            { _ -> },
        ).also { check(it == 19) }
    }.join()

    // A poll that throws before handing off its notification must not leave an
    // active cancellation registration or wait forever for a nonexistent wake.
    for (failure in listOf(IllegalStateException("poll failed"), CancellationException("poll failed"))) {
        var failureFrees = 0
        var failureCancels = 0
        val caller = launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                awaitMockRustFuture<Unit>(
                    { 20L },
                    { _, _ -> throw failure },
                    { _ -> failureCancels++ },
                    { _ -> error("complete after failed poll") },
                    { _ -> failureFrees++ },
                )
                error("poll failure was swallowed")
            } catch (caught: Throwable) {
                check(caught::class == failure::class && caught.message == failure.message)
            }
            // Cancellation after free must never touch the native handle.
            currentCoroutineContext()[Job]!!.cancel()
        }
        caller.join()
        check(failureFrees == 1 && failureCancels == 0)
    }
}

private suspend fun checkTerminalRelease() = coroutineScope {
    // Preserve the externally visible try/finally contract for every exit:
    // free exactly once, and a free failure overrides the earlier exception.
    for (stage in listOf("ready", "complete", "complete-cancel", "poll", "poll-cancel")) {
        for (freeFails in listOf(false, true)) {
            val events = mutableListOf<String>()
            var frees = 0
            var cancels = 0
            val original = if (stage.endsWith("cancel")) CancellationException(stage)
                else IllegalArgumentException(stage)
            val freeFailure = IllegalStateException("free failed")
            val caller = launch(start = CoroutineStart.UNDISPATCHED) {
                val thread = Thread.currentThread()
                var delivered = false
                var caught: Throwable? = null
                try {
                    val value = awaitMockRustFuture(
                        { 21L },
                        { _, _ ->
                            events.add("poll")
                            if (stage.startsWith("poll")) throw original
                            true
                        },
                        { _ -> cancels++ },
                        { _ ->
                            check(Thread.currentThread() === thread)
                            events.add("complete")
                            if (stage.startsWith("complete")) throw original
                            87
                        },
                        { _ ->
                            check(Thread.currentThread() === thread)
                            events.add("free")
                            frees++
                            if (freeFails) throw freeFailure
                        },
                    )
                    check(value == 87)
                    delivered = true
                } catch (error: Throwable) { caught = error }
                val expected = if (freeFails) freeFailure else if (stage == "ready") null else original
                check(delivered == (expected == null))
                check(caught?.javaClass == expected?.javaClass && caught?.message == expected?.message)
                // A failed free must still close admission to cancellation;
                // neither cancellation nor the outer finally may retry it.
                currentCoroutineContext()[Job]!!.cancel()
            }
            caller.join()
            check(frees == 1 && cancels == 0)
            check(events == if (stage.startsWith("poll")) listOf("poll", "free")
                else listOf("poll", "complete", "free"))
        }
    }

    for (freeFails in listOf(false, true)) {
        var completes = 0
        var frees = 0
        val cancels = java.util.concurrent.atomic.AtomicInteger()
        var closed = false
        var delivered = false
        val caller = launch(start = CoroutineStart.UNDISPATCHED) {
            val owner = currentCoroutineContext()[Job]!!
            val finishedCancel = java.util.concurrent.CountDownLatch(1)
            val cancelFailure = java.util.concurrent.atomic.AtomicReference<Throwable>()
            var producer: Thread? = null
            try {
                val value = awaitMockRustFuture(
                    { 22L }, { _, _ -> true },
                    { _ ->
                        check(!closed) { "cancel accessed a terminal handle" }
                        cancels.incrementAndGet()
                    },
                    { _ ->
                        completes++
                        closed = true
                        producer = Thread({
                            try { owner.cancel() }
                            catch (error: Throwable) { cancelFailure.set(error) }
                            finally { finishedCancel.countDown() }
                        }, "jni-opt5-terminal-cancel").apply { isDaemon = true; start() }
                        check(finishedCancel.await(5, java.util.concurrent.TimeUnit.SECONDS)) {
                            "terminal cancellation did not return"
                        }
                        check(cancelFailure.get() == null)
                        93
                    },
                    { _ ->
                        check(closed)
                        frees++
                        owner.cancel()
                        if (freeFails) throw IllegalStateException("terminal free failed")
                    },
                )
                check(value == 93 && !freeFails)
                delivered = true
            } catch (error: IllegalStateException) {
                check(freeFails && error.message == "terminal free failed")
            } finally {
                producer?.join(5_000)
                check(producer?.isAlive == false)
            }
        }
        caller.join()
        check(completes == 1 && frees == 1 && cancels.get() == 0)
        check(delivered == !freeFails)
    }
}


// Observe the official Job implementation; no replacement continuation state.
@OptIn(InternalCoroutinesApi::class)
private class RecordingJob(private val delegate: Job) : Job by delegate {
    var registrations = 0
    var disposals = 0
    var callbacks = 0
    var beforeHandler: () -> Unit = {}
    var onDispose: () -> Unit = {}

    @Suppress("UNCHECKED_CAST")
    override fun <E : kotlin.coroutines.CoroutineContext.Element> get(
        key: kotlin.coroutines.CoroutineContext.Key<E>,
    ): E? = if (key == Job) this as E else null
    override fun <R> fold(initial: R, operation: (R, kotlin.coroutines.CoroutineContext.Element) -> R): R =
        operation(initial, this)
    override fun minusKey(key: kotlin.coroutines.CoroutineContext.Key<*>): kotlin.coroutines.CoroutineContext =
        if (key == Job) kotlin.coroutines.EmptyCoroutineContext else this

    override fun invokeOnCompletion(handler: CompletionHandler): DisposableHandle =
        error("await used the default completion overload")

    override fun invokeOnCompletion(
        onCancelling: Boolean, invokeImmediately: Boolean, handler: CompletionHandler,
    ): DisposableHandle {
        check(onCancelling && invokeImmediately)
        registrations++
        val handle = delegate.invokeOnCompletion(onCancelling, invokeImmediately) { cause ->
            callbacks++
            beforeHandler()
            handler(cause)
        }
        return object : DisposableHandle {
            override fun dispose() {
                disposals++
                onDispose()
                handle.dispose()
            }
        }
    }
}

// Standard startCoroutine permits the exact caller context, including absent or
// completed Jobs, without a launch builder replacing it with a child Job.
private class ProtocolCall<T>(context: kotlin.coroutines.CoroutineContext, body: suspend () -> T) {
    private val result = java.util.concurrent.atomic.AtomicReference<Result<T>>()
    private val completions = java.util.concurrent.atomic.AtomicInteger()
    val done = java.util.concurrent.CountDownLatch(1)
    init {
        body.startCoroutine(object : kotlin.coroutines.Continuation<T> {
            override val context = context
            override fun resumeWith(result: Result<T>) {
                check(completions.incrementAndGet() == 1)
                this@ProtocolCall.result.set(result)
                done.countDown()
            }
        })
    }
    fun result(): Result<T> {
        check(done.await(5, java.util.concurrent.TimeUnit.SECONDS)) { "protocol call did not finish" }
        return checkNotNull(result.get())
    }
    fun pending() { check(done.count == 1L) { "call finished early: ${result.get()}" } }
}

private fun java.util.concurrent.CountDownLatch.awaitGate() {
    check(await(5, java.util.concurrent.TimeUnit.SECONDS)) { "protocol latch timed out" }
}

private fun checkNotificationPublication() {
    var cases = 0
    for (publishFirst in listOf(false, true)) for (outcome in 0..2) {
        val owner = Job()
        val job = RecordingJob(owner)
        val error = IllegalArgumentException("original notification error")
        val notification = when (outcome) {
            0 -> Result.success(false)
            1 -> Result.success(true)
            else -> Result.failure(error)
        }
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        var polls = 0
        var cancels = 0
        var completes = 0
        var frees = 0
        fun publish() {
            check(polls == 1 && completes == 0 && frees == 0)
            owner.cancel()
            check(cancels == 1)
            waiter!!.resumeWith(notification)
            check(runCatching { waiter!!.resumeWith(Result.success(true)) }.exceptionOrNull() is IllegalStateException)
        }
        val call = ProtocolCall(CoroutineName("original-notification") + job) {
            awaitMockRustFuture(
                { 30L },
                { _, cont ->
                    check(cont.context[Job] === job)
                    polls++
                    if (polls == 1) {
                        waiter = cont
                        if (publishFirst) publish()
                        false
                    } else { check(outcome == 0 && polls == 2); true }
                },
                { _ -> cancels++ }, { _ -> completes++; 71 }, { _ -> frees++ },
            )
        }
        if (!publishFirst) { call.pending(); publish() }
        val received = call.result()
        if (outcome == 2) check(received.exceptionOrNull() === error)
        else check(received.getOrThrow() == 71)
        check(polls == if (outcome == 0) 2 else 1)
        check(completes == if (outcome == 2) 0 else 1)
        check(frees == 1 && job.registrations == 1 && job.disposals == 1 && job.callbacks == 1)
        cases++
    }
    check(cases == 6)
    println("jni opt9 notification: 6 actual helper early/suspended Wake/Ready/failure scenarios passed")
}

private fun checkSingleRegistration() {
    var cases = 0
    for (mode in listOf("ready", "wakes", "cancel-cleanup", "poll-error", "poll-cancel-error", "terminal-error")) {
        val owner = Job()
        val job = RecordingJob(owner)
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        var polls = 0
        var cancels = 0
        var completes = 0
        var frees = 0
        val failure = if (mode == "poll-cancel-error") CancellationException(mode) else IllegalStateException(mode)
        val call = ProtocolCall(job) {
            awaitMockRustFuture(
                { 31L },
                { _, cont ->
                    polls++
                    check(job.registrations == 1 && job.disposals == 0 && owner.children.none())
                    if (mode.startsWith("poll-")) throw failure
                    if (mode == "wakes" || mode == "cancel-cleanup") {
                        waiter = cont
                        false
                    } else true
                },
                { _ -> cancels++ },
                { _ -> completes++; if (mode == "terminal-error") throw failure; 72 },
                { _ -> frees++ },
            )
        }
        if (mode == "wakes" || mode == "cancel-cleanup") {
            call.pending()
            if (mode == "cancel-cleanup") owner.cancel()
            repeat(3) {
                waiter!!.resumeWith(Result.success(false))
                call.pending()
                check(completes == 0 && frees == 0 && job.disposals == 0 && job.registrations == 1)
                if (mode == "cancel-cleanup") owner.cancel()
            }
            waiter!!.resumeWith(Result.success(true))
        }
        val result = call.result()
        if (mode.contains("error")) check(result.exceptionOrNull() === failure)
        else check(result.getOrThrow() == 72)
        check(polls == if (mode == "wakes" || mode == "cancel-cleanup") 4 else 1)
        check(completes == if (mode.startsWith("poll-")) 0 else 1)
        check(frees == 1 && job.registrations == 1 && job.disposals == 1)
        owner.cancel()
        check(cancels == if (mode == "cancel-cleanup") 1 else 0)
        cases++
    }
    println("jni opt9 registration: $cases single registration/disposal scenarios passed")
}

private suspend fun checkJobBoundaries() = coroutineScope {
    var cases = 0
    for (mode in listOf("no-job", "noncancellable", "cancelled-complete", "cancelling", "completed", "pending-complete", "factory-cancel")) {
        val owner = Job()
        val cleanup = CompletableDeferred<Unit>()
        val child = if (mode == "cancelling") CoroutineScope(owner).launch(start = CoroutineStart.UNDISPATCHED) {
            try { awaitCancellation() } finally { withContext(NonCancellable) { cleanup.await() } }
        } else null
        if (mode == "cancelled-complete" || mode == "cancelling") owner.cancel()
        if (mode == "completed") check(owner.complete())
        if (mode == "cancelling") check(owner.isCancelled && !owner.isCompleted)
        if (mode == "cancelled-complete") check(owner.isCompleted)
        val job = RecordingJob(owner)
        val context = when (mode) {
            "no-job" -> kotlin.coroutines.EmptyCoroutineContext
            "noncancellable" -> NonCancellable
            else -> job
        }
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        var created = false
        var cancels = 0
        var frees = 0
        val expectedCancel = mode != "no-job" && mode != "noncancellable"
        val call = ProtocolCall(context) {
            awaitMockRustFuture(
                { if (mode == "factory-cancel") owner.cancel(); created = true; 32L },
                { _, cont ->
                    check(cancels == if (expectedCancel && mode != "pending-complete") 1 else 0)
                    if (mode == "pending-complete") { waiter = cont; false } else true
                },
                { _ -> check(created); cancels++ }, { _ -> 73 }, { _ -> frees++ },
            )
        }
        if (mode == "pending-complete") {
            call.pending()
            check(owner.complete())
            check(cancels == 1 && frees == 0)
            call.pending()
            waiter!!.resumeWith(Result.success(true))
        }
        check(call.result().getOrThrow() == 73 && frees == 1)
        check(cancels == if (expectedCancel) 1 else 0)
        check(job.registrations == if (expectedCancel) 1 else 0)
        check(job.disposals == job.registrations)
        cleanup.complete(Unit)
        child?.join()
        owner.cancel()
        cases++
    }
    println("jni opt9 Job boundaries: $cases scenarios passed")
}

@OptIn(InternalCoroutinesApi::class)
private fun checkCancellationReporting() {
    for (immediate in listOf(false, true)) {
        val owner = Job()
        if (immediate) owner.cancel()
        val job = RecordingJob(owner)
        val failure = IllegalArgumentException("controlled Kotlin cancel failure")
        val reports = mutableListOf<Pair<kotlin.coroutines.CoroutineContext, Throwable>>()
        val handler = CoroutineExceptionHandler { context, error -> reports += context to error }
        val context = CoroutineName("caller-report") + handler + job
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        var completes = 0
        var frees = 0
        val call = ProtocolCall(context) {
            awaitMockRustFuture(
                { 33L }, { _, cont -> waiter = cont; false }, { _ -> throw failure },
                { _ -> completes++; 74 }, { _ -> frees++ },
            )
        }
        call.pending()
        if (!immediate) owner.cancel()
        check(reports.size == 1)
        check(reports.single().first === context)
        check(reports.single().second is CompletionHandlerException && reports.single().second.cause === failure)
        check(completes == 0 && frees == 0)
        call.pending()
        // Controlled test notification, not a promise of native-panic recovery.
        waiter!!.resumeWith(Result.success(true))
        check(call.result().getOrThrow() == 74 && completes == 1 && frees == 1)
        check(job.registrations == 1 && job.disposals == 1)
    }
    println("jni opt9 cancellation reporting: 2 immediate/deferred original context/cause scenarios passed")
}

private fun checkCancelledPollFailure() {
    var cases = 0
    for (cancellationError in listOf(false, true)) for (freeFails in listOf(false, true)) {
        val owner = Job()
        val job = RecordingJob(owner)
        val failure = if (cancellationError) CancellationException("poll failure") else IllegalStateException("poll failure")
        val freeFailure = IllegalArgumentException("fallback free failure")
        var cancels = 0
        var frees = 0
        val call = ProtocolCall(job) {
            awaitMockRustFuture<Unit>(
                { 34L }, { _, _ -> owner.cancel(); check(cancels == 1); throw failure },
                { _ -> cancels++ }, { _ -> error("complete after failed handoff") },
                { _ -> frees++; if (freeFails) throw freeFailure },
            )
        }
        check(call.result().exceptionOrNull() === if (freeFails) freeFailure else failure)
        check(cancels == 1 && frees == 1 && job.registrations == 1 && job.disposals == 1)
        cases++
    }
    println("jni opt9 cancelled poll failure: $cases original error/free priority scenarios passed")
}

private fun checkLiveCancellationGate() {
    // An official Job handler is already executing, but paused before entry into
    // the helper. Terminal must finish and dispose without waiting for it.
    run {
        val owner = Job()
        val job = RecordingJob(owner)
        val obtained = java.util.concurrent.CountDownLatch(1)
        val enter = java.util.concurrent.CountDownLatch(1)
        val finished = java.util.concurrent.CountDownLatch(1)
        val threadFailure = java.util.concurrent.atomic.AtomicReference<Throwable>()
        job.beforeHandler = { obtained.countDown(); enter.awaitGate() }
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        var cancels = 0
        var freed = false
        job.onDispose = { check(freed && finished.count == 1L) }
        val call = ProtocolCall(job) {
            awaitMockRustFuture(
                { 35L }, { _, cont -> waiter = cont; false },
                { _ -> check(!freed); cancels++ }, { _ -> 75 }, { _ -> freed = true },
            )
        }
        val producer = Thread({
            try { owner.cancel() } catch (error: Throwable) { threadFailure.set(error) }
            finally { finished.countDown() }
        }, "jni-opt9-handler-before-entry")
        producer.start()
        try {
            obtained.awaitGate()
            waiter!!.resumeWith(Result.success(true))
            check(call.result().getOrThrow() == 75 && freed && job.disposals == 1)
        } finally { enter.countDown(); producer.join(5_000) }
        check(!producer.isAlive && threadFailure.get() == null && cancels == 0)
    }
    // A native cancel admitted under live's monitor must finish before terminal.
    // Observe the actual monitor contention, then release the causal latch.
    run {
        val owner = Job()
        val job = RecordingJob(owner)
        val inCancel = java.util.concurrent.CountDownLatch(1)
        val leaveCancel = java.util.concurrent.CountDownLatch(1)
        val cancelExited = java.util.concurrent.atomic.AtomicBoolean(false)
        val completes = java.util.concurrent.atomic.AtomicInteger()
        val frees = java.util.concurrent.atomic.AtomicInteger()
        val failure = java.util.concurrent.atomic.AtomicReference<Throwable>()
        var waiter: kotlin.coroutines.Continuation<Boolean>? = null
        val call = ProtocolCall(job) {
            awaitMockRustFuture(
                { 36L }, { _, cont -> waiter = cont; false },
                { _ -> inCancel.countDown(); leaveCancel.awaitGate(); cancelExited.set(true) },
                { _ -> check(cancelExited.get()); completes.incrementAndGet(); 76 },
                { _ -> check(cancelExited.get()); frees.incrementAndGet() },
            )
        }
        val cancelThread = Thread({
            try { owner.cancel() } catch (error: Throwable) { failure.set(error) }
        }, "jni-opt9-admitted-cancel")
        val readyThread = Thread({
            try { waiter!!.resumeWith(Result.success(true)) } catch (error: Throwable) { failure.set(error) }
        }, "jni-opt9-terminal-waits")
        cancelThread.start()
        try {
            inCancel.awaitGate()
            readyThread.start()
            val deadline = System.nanoTime() + java.util.concurrent.TimeUnit.SECONDS.toNanos(5)
            while (readyThread.state != Thread.State.BLOCKED && readyThread.isAlive && System.nanoTime() < deadline) Thread.yield()
            check(readyThread.state == Thread.State.BLOCKED) { "terminal did not wait for admitted native cancel" }
            check(completes.get() == 0 && frees.get() == 0)
            call.pending()
        } finally {
            leaveCancel.countDown()
            cancelThread.join(5_000)
            readyThread.join(5_000)
        }
        check(!cancelThread.isAlive && !readyThread.isAlive && failure.get() == null)
        check(call.result().getOrThrow() == 76 && completes.get() == 1 && frees.get() == 1)
        check(job.registrations == 1 && job.disposals == 1)
    }
    println("jni opt9 live lock: 2 handler-before-entry/admitted-cancel scenarios passed")
}
