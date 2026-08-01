/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.yield
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import uniffi.uniffi_output_streams.InternalException
import uniffi.uniffi_output_streams.OutputStreamException
import uniffi.uniffi_output_streams.ProbeSnapshot
import uniffi.uniffi_output_streams.UniFfiStream
import uniffi.uniffi_output_streams.countingStream
import uniffi.uniffi_output_streams.optionalStream
import uniffi.uniffi_output_streams.pendingStream
import uniffi.uniffi_output_streams.probeSnapshot
import uniffi.uniffi_output_streams.resetProbe
import uniffi.uniffi_output_streams.typedErrorStream

sealed class ErrorMessageCompileProbe : kotlin.Exception() {
    class StringMessage(
        override val `message`: kotlin.String,
    ) : ErrorMessageCompileProbe()

    class NullableStringMessage(
        override val `message`: kotlin.String?,
    ) : ErrorMessageCompileProbe()

    class NumericMessage(
        val `messageValue2`: kotlin.UInt,
        val `messageValue`: kotlin.UInt,
    ) : ErrorMessageCompileProbe() {
        override val message
            get() = "messageValue2=${ `messageValue2` }, messageValue=${ `messageValue` }"
    }

    class NoMessage(
        val `code`: kotlin.UInt,
    ) : ErrorMessageCompileProbe() {
        override val message
            get() = "code=${ `code` }"
    }
}

run {
    val stringMessage: Throwable = ErrorMessageCompileProbe.StringMessage("message")
    check(stringMessage.message == "message")

    val nullableMessage: Throwable = ErrorMessageCompileProbe.NullableStringMessage(null)
    check(nullableMessage.message == null)

    val numericMessage = ErrorMessageCompileProbe.NumericMessage(42U, 7U)
    check(numericMessage.messageValue2 == 42U)
    check(numericMessage.messageValue == 7U)
    check(numericMessage.message == "messageValue2=42, messageValue=7")

    val noMessage = ErrorMessageCompileProbe.NoMessage(9U)
    check(noMessage.code == 9U)
    check(noMessage.message == "code=9")
}

suspend fun awaitProbe(
    probeId: String,
    predicate: (ProbeSnapshot) -> Boolean,
): ProbeSnapshot = withTimeout(10_000L) {
    var snapshot = probeSnapshot(probeId)
    while (!predicate(snapshot)) {
        yield()
        snapshot = probeSnapshot(probeId)
    }
    snapshot
}

suspend fun requireStableProbe(probeId: String, expected: ProbeSnapshot) {
    repeat(16) {
        yield()
        check(probeSnapshot(probeId) == expected) {
            "probe $probeId changed after reaching a terminal state"
        }
    }
}

suspend fun requireSingleUseFailure(block: suspend () -> Unit) {
    try {
        block()
        error("expected a repeated/concurrent collect to fail")
    } catch (error: InternalException) {
        check(error.message == "UniFFI output streams may only be consumed once")
    }
}

runBlocking {
    val probeId = "kotlin-lazy-and-repeated"
    resetProbe(probeId)

    val stream: UniFfiStream<UInt> = countingStream(probeId, 2U)
    val flow: Flow<UInt> = stream
    check(probeSnapshot(probeId).streamStarts == 0UL)

    check(flow.toList() == listOf(0U, 1U))
    val done = probeSnapshot(probeId)
    check(done.streamStarts == 1UL)
    check(done.streamNextPolls == 3UL)
    check(done.streamTerminalDrops == 1UL)
    check(done.streamCancelledDrops == 0UL)
    check(done.streamDrops == 1UL)
    requireStableProbe(probeId, done)

    requireSingleUseFailure { stream.toList() }
    check(probeSnapshot(probeId) == done)
}

runBlocking {
    val probeId = "kotlin-concurrent-and-pending-cancel"
    resetProbe(probeId)
    val stream = pendingStream(probeId)
    check(probeSnapshot(probeId).streamStarts == 0UL)

    val collecting: Job = launch {
        stream.collect { error("pending stream unexpectedly emitted $it") }
    }
    val pending = awaitProbe(probeId) {
        it.streamStarts == 1UL && it.streamNextPolls == 1UL
    }
    check(pending.streamDrops == 0UL)

    requireSingleUseFailure { stream.collect {} }
    check(probeSnapshot(probeId).streamStarts == 1UL)
    check(probeSnapshot(probeId).streamNextPolls == 1UL)

    collecting.cancelAndJoin()
    val cancelled = awaitProbe(probeId) { it.streamDrops == 1UL }
    check(cancelled.streamStarts == 1UL)
    check(cancelled.streamNextPolls == 1UL)
    check(cancelled.streamTerminalDrops == 0UL)
    check(cancelled.streamCancelledDrops == 1UL)
    requireStableProbe(probeId, cancelled)
}

runBlocking {
    val probeId = "kotlin-pull-backpressure"
    resetProbe(probeId)
    val stream = countingStream(probeId, 3U)
    val firstItemEntered = CompletableDeferred<Unit>()
    val releaseFirstItem = CompletableDeferred<Unit>()
    val values = mutableListOf<UInt>()

    val collecting = launch {
        stream.collect { value ->
            values.add(value)
            if (value == 0U) {
                firstItemEntered.complete(Unit)
                releaseFirstItem.await()
            }
        }
    }
    withTimeout(10_000L) { firstItemEntered.await() }

    repeat(16) {
        yield()
        val suspended = probeSnapshot(probeId)
        check(suspended.streamStarts == 1UL)
        check(suspended.streamNextPolls == 1UL) {
            "output stream prefetched while its collector was suspended"
        }
    }

    releaseFirstItem.complete(Unit)
    collecting.join()
    check(values == listOf(0U, 1U, 2U))
    val done = probeSnapshot(probeId)
    check(done.streamNextPolls == 4UL)
    check(done.streamTerminalDrops == 1UL)
    check(done.streamDrops == 1UL)
    requireStableProbe(probeId, done)
}

runBlocking {
    val probeId = "kotlin-optional-item"
    resetProbe(probeId)

    val values: List<UInt?> = optionalStream(probeId).toList()
    check(values == listOf(1U, null, 2U))
    val done = probeSnapshot(probeId)
    check(done.streamStarts == 1UL)
    check(done.streamNextPolls == 4UL)
    check(done.streamTerminalDrops == 1UL)
    check(done.streamDrops == 1UL)
}

runBlocking {
    val probeId = "kotlin-typed-error"
    resetProbe(probeId)
    val values = mutableListOf<UInt>()
    val stream = typedErrorStream(probeId)

    try {
        stream.collect { values.add(it) }
        error("typed error stream unexpectedly completed")
    } catch (error: OutputStreamException.Detailed) {
        check(error.code == 42U)
        check(error.message == "typed output stream failure")
    }

    check(values == listOf(7U))
    val failed = probeSnapshot(probeId)
    check(failed.streamStarts == 1UL)
    check(failed.streamNextPolls == 2UL)
    check(failed.streamTerminalDrops == 1UL)
    check(failed.streamCancelledDrops == 0UL)
    check(failed.streamDrops == 1UL)
    requireStableProbe(probeId, failed)
    requireSingleUseFailure { stream.collect {} }
    check(probeSnapshot(probeId) == failed)
}

runBlocking {
    val probeId = "kotlin-take-one"
    resetProbe(probeId)

    check(countingStream(probeId, 5U).take(1).toList() == listOf(0U))
    val cancelled = probeSnapshot(probeId)
    check(cancelled.streamStarts == 1UL)
    check(cancelled.streamNextPolls == 1UL)
    check(cancelled.streamTerminalDrops == 0UL)
    check(cancelled.streamCancelledDrops == 1UL)
    check(cancelled.streamDrops == 1UL)
    requireStableProbe(probeId, cancelled)
}
