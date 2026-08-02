/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.yield
import uniffi.uniffi_output_streams.ProbeSnapshot
import uniffi.uniffi_output_streams.probeSnapshot
import uniffi.uniffi_output_streams.releaseInputStreamConsumer
import uniffi.uniffi_output_streams.rendezvousInputStreamSum
import uniffi.uniffi_output_streams.resetProbe
import uniffi.uniffi_output_streams.uniffiInputStreamHandleCountUniffiOutputStreams

suspend fun awaitInputProbe(
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

runBlocking {
    val probeId = "kotlin-input-rendezvous-backpressure"
    resetProbe(probeId)

    val producerEntered = CompletableDeferred<Unit>()
    val releaseFirstItem = CompletableDeferred<Unit>()
    val secondEmitEntered = CompletableDeferred<Unit>()
    val secondEmitReturned = CompletableDeferred<Unit>()
    val input: Flow<UInt> = flow {
        producerEntered.complete(Unit)
        releaseFirstItem.await()
        emit(1U)

        secondEmitEntered.complete(Unit)
        emit(2U)
        secondEmitReturned.complete(Unit)
    }

    val consuming = async { rendezvousInputStreamSum(probeId, input) }
    withTimeout(10_000L) { producerEntered.await() }

    val firstRequest = awaitInputProbe(probeId) {
        it.inputStreamStarts == 1UL &&
            it.inputStreamNextRequests == 1UL &&
            it.inputStreamItems == 0UL
    }
    check(firstRequest.inputStreamTerminalCompletions == 0UL)
    check(uniffiInputStreamHandleCountUniffiOutputStreams() == 1)

    repeat(16) {
        yield()
        val blockedProducer = probeSnapshot(probeId)
        check(blockedProducer.inputStreamStarts == 1UL)
        check(blockedProducer.inputStreamNextRequests == 1UL) {
            "Rust requested more than one input item while the producer was suspended"
        }
        check(blockedProducer.inputStreamItems == 0UL)
        check(!secondEmitEntered.isCompleted)
    }

    releaseFirstItem.complete(Unit)
    withTimeout(10_000L) { secondEmitEntered.await() }
    val firstItem = awaitInputProbe(probeId) { it.inputStreamItems == 1UL }
    check(firstItem.inputStreamNextRequests == 1UL)
    check(firstItem.inputStreamTerminalCompletions == 0UL)

    repeat(16) {
        yield()
        val blockedConsumer = probeSnapshot(probeId)
        check(blockedConsumer.inputStreamStarts == 1UL)
        check(blockedConsumer.inputStreamNextRequests == 1UL) {
            "Rust prefetched a second input item while its consumer gate was suspended"
        }
        check(blockedConsumer.inputStreamItems == 1UL)
        check(!secondEmitReturned.isCompleted) {
            "Kotlin emitted a second item without a matching Rust pull"
        }
    }

    releaseInputStreamConsumer(probeId)
    withTimeout(10_000L) { secondEmitReturned.await() }
    check(withTimeout(10_000L) { consuming.await() } == 3UL)

    val completed = probeSnapshot(probeId)
    check(completed.inputStreamStarts == 1UL)
    check(completed.inputStreamNextRequests == 3UL)
    check(completed.inputStreamItems == 2UL)
    check(completed.inputStreamTerminalCompletions == 1UL)
    check(uniffiInputStreamHandleCountUniffiOutputStreams() == 0)
}
