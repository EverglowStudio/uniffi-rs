import Foundation
import uniffi_output_streams

private func waitUntil(
    _ description: String,
    _ condition: @escaping () -> Bool
) async {
    for _ in 0..<10_000 {
        if condition() {
            return
        }
        await Task.yield()
    }
    fatalError("timed out waiting for \(description)")
}

private func testCountingStreamIsLazyAndPullBased() async throws {
    let probeId = "swift-counting-lazy-pull"
    resetProbe(probeId: probeId)

    let stream: UniFfiStream<UInt32> = countingStream(probeId: probeId, count: 2)
    assert(probeSnapshot(probeId: probeId).streamStarts == 0)
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 0)

    var iterator = stream.makeAsyncIterator()
    assert(probeSnapshot(probeId: probeId).streamStarts == 0)
    await Task.yield()
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 0)

    let firstItem = try await iterator.next()
    assert(firstItem == 0)
    assert(probeSnapshot(probeId: probeId).streamStarts == 1)
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 1)
    await Task.yield()
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 1)

    let secondItem = try await iterator.next()
    assert(secondItem == 1)
    let done = try await iterator.next()
    assert(done == nil)
    let terminal = probeSnapshot(probeId: probeId)
    assert(terminal.streamNextPolls == 3)
    assert(terminal.streamDrops == 1)
    assert(terminal.streamTerminalDrops == 1)
    assert(terminal.streamCancelledDrops == 0)

    let doneAgain = try await iterator.next()
    assert(doneAgain == nil)
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 3)
}

private func testOptionalAndTypedErrorStreamSteps() async throws {
    let optionalProbeId = "swift-optional-item"
    resetProbe(probeId: optionalProbeId)
    let optional: UniFfiStream<UInt32?> = optionalStream(probeId: optionalProbeId)
    var optionalIterator = optional.makeAsyncIterator()

    let first = try await optionalIterator.next()
    guard case let .some(.some(firstValue)) = first, firstValue == 1 else {
        fatalError("expected Item(Some(1))")
    }
    let second = try await optionalIterator.next()
    guard case .some(.none) = second else {
        fatalError("expected Item(None), not Done")
    }
    let third = try await optionalIterator.next()
    guard case let .some(.some(thirdValue)) = third, thirdValue == 2 else {
        fatalError("expected Item(Some(2))")
    }
    let optionalDone = try await optionalIterator.next()
    assert(optionalDone == nil)
    let optionalTerminal = probeSnapshot(probeId: optionalProbeId)
    assert(optionalTerminal.streamStarts == 1)
    assert(optionalTerminal.streamNextPolls == 4)
    assert(optionalTerminal.streamDrops == 1)
    assert(optionalTerminal.streamTerminalDrops == 1)

    let errorProbeId = "swift-typed-error"
    resetProbe(probeId: errorProbeId)
    let errorStream: UniFfiStream<UInt32> = typedErrorStream(probeId: errorProbeId)
    var errorIterator = errorStream.makeAsyncIterator()
    let errorItem = try await errorIterator.next()
    assert(errorItem == 7)
    do {
        _ = try await errorIterator.next()
        fatalError("expected typed output stream error")
    } catch let error as OutputStreamError {
        guard case let .Detailed(code: code, message: message) = error,
              code == 42,
              message == "typed output stream failure" else {
            fatalError("unexpected typed output stream error: \(error)")
        }
    }
    let errorDone = try await errorIterator.next()
    assert(errorDone == nil)
    let errorTerminal = probeSnapshot(probeId: errorProbeId)
    assert(errorTerminal.streamStarts == 1)
    assert(errorTerminal.streamNextPolls == 2)
    assert(errorTerminal.streamDrops == 1)
    assert(errorTerminal.streamTerminalDrops == 1)
    assert(errorTerminal.streamCancelledDrops == 0)
}

private func testSingleUseAndIdleCleanup() async throws {
    let probeId = "swift-single-use-before-start"
    resetProbe(probeId: probeId)
    let stream: UniFfiStream<UInt32> = countingStream(probeId: probeId, count: 1)
    var accepted = stream.makeAsyncIterator()
    var rejected = stream.makeAsyncIterator()

    do {
        _ = try await rejected.next()
        fatalError("expected second iterator failure")
    } catch {
        // AsyncSequence cannot throw from makeAsyncIterator(), so the rejected
        // iterator reports the deterministic consumed error from its first next().
        guard error.localizedDescription == "UniFFI output streams may only be consumed once" else {
            fatalError("unexpected second iterator error: \(error)")
        }
    }
    assert(probeSnapshot(probeId: probeId).streamStarts == 0)
    assert(probeSnapshot(probeId: probeId).streamNextPolls == 0)

    stream.cancel()
    stream.cancel()
    let idleDone = try await accepted.next()
    assert(idleDone == nil)
    let idleCancelled = probeSnapshot(probeId: probeId)
    assert(idleCancelled.streamStarts == 0)
    assert(idleCancelled.streamNextPolls == 0)
    assert(idleCancelled.streamDrops == 0)

    let dropProbeId = "swift-idle-drop"
    resetProbe(probeId: dropProbeId)
    do {
        let idle: UniFfiStream<UInt32> = countingStream(probeId: dropProbeId, count: 1)
        withExtendedLifetime(idle) {}
    }
    await Task.yield()
    let idleDrop = probeSnapshot(probeId: dropProbeId)
    assert(idleDrop.streamStarts == 0)
    assert(idleDrop.streamNextPolls == 0)
    assert(idleDrop.streamDrops == 0)
}

private func consumeOneThenDrop(probeId: String) async throws {
    let stream: UniFfiStream<UInt32> = countingStream(probeId: probeId, count: 2)
    var iterator = stream.makeAsyncIterator()
    let firstItem = try await iterator.next()
    assert(firstItem == 0)
}

private func testActiveCleanupAndPendingCancellation() async throws {
    let earlyProbeId = "swift-early-drop"
    resetProbe(probeId: earlyProbeId)
    try await consumeOneThenDrop(probeId: earlyProbeId)
    await waitUntil("early stream drop") {
        probeSnapshot(probeId: earlyProbeId).streamDrops == 1
    }
    let early = probeSnapshot(probeId: earlyProbeId)
    assert(early.streamStarts == 1)
    assert(early.streamNextPolls == 1)
    assert(early.streamDrops == 1)
    assert(early.streamTerminalDrops == 0)
    assert(early.streamCancelledDrops == 1)

    let pendingProbeId = "swift-pending-cancel"
    resetProbe(probeId: pendingProbeId)
    let pending: UniFfiStream<UInt32> = pendingStream(probeId: pendingProbeId)
    var firstIterator = pending.makeAsyncIterator()
    var secondIterator = firstIterator
    let firstNext = Task { () throws -> UInt32? in
        try await firstIterator.next()
    }
    await waitUntil("pending stream first poll") {
        let snapshot = probeSnapshot(probeId: pendingProbeId)
        return snapshot.streamStarts == 1 && snapshot.streamNextPolls == 1
    }

    do {
        _ = try await secondIterator.next()
        fatalError("expected concurrent next failure")
    } catch {
        // The first pending next owns the only in-flight native next future.
        guard error.localizedDescription == "UniFFI output stream received a concurrent next() call" else {
            fatalError("unexpected concurrent next error: \(error)")
        }
    }
    assert(probeSnapshot(probeId: pendingProbeId).streamNextPolls == 1)

    firstNext.cancel()
    firstNext.cancel()
    do {
        _ = try await firstNext.value
        fatalError("cancelled stream next unexpectedly succeeded")
    } catch is CancellationError {
        // Expected: CALL_CANCELLED maps to Swift CancellationError.
    }
    pending.cancel()
    pending.cancel()
    await waitUntil("pending stream cancellation drop") {
        probeSnapshot(probeId: pendingProbeId).streamDrops == 1
    }
    let cancelled = probeSnapshot(probeId: pendingProbeId)
    assert(cancelled.streamStarts == 1)
    assert(cancelled.streamNextPolls == 1)
    assert(cancelled.streamDrops == 1)
    assert(cancelled.streamTerminalDrops == 0)
    assert(cancelled.streamCancelledDrops == 1)
    let cancelledDone = try await secondIterator.next()
    assert(cancelledDone == nil)
    assert(probeSnapshot(probeId: pendingProbeId).streamNextPolls == 1)

    let explicitProbeId = "swift-explicit-active-cancel"
    resetProbe(probeId: explicitProbeId)
    let explicit: UniFfiStream<UInt32> = pendingStream(probeId: explicitProbeId)
    var explicitIterator = explicit.makeAsyncIterator()
    let explicitNext = Task { () throws -> UInt32? in
        try await explicitIterator.next()
    }
    await waitUntil("explicit stream first poll") {
        let snapshot = probeSnapshot(probeId: explicitProbeId)
        return snapshot.streamStarts == 1 && snapshot.streamNextPolls == 1
    }
    explicit.cancel()
    explicit.cancel()
    do {
        let explicitResult = try await explicitNext.value
        assert(explicitResult == nil)
    } catch {
        fatalError("unexpected explicitly cancelled stream error: \(error)")
    }
    await waitUntil("explicit stream cancellation drop") {
        probeSnapshot(probeId: explicitProbeId).streamDrops == 1
    }
    let explicitlyCancelled = probeSnapshot(probeId: explicitProbeId)
    assert(explicitlyCancelled.streamStarts == 1)
    assert(explicitlyCancelled.streamNextPolls == 1)
    assert(explicitlyCancelled.streamDrops == 1)
    assert(explicitlyCancelled.streamTerminalDrops == 0)
    assert(explicitlyCancelled.streamCancelledDrops == 1)
    let explicitDone = try await explicitIterator.next()
    assert(explicitDone == nil)
    assert(probeSnapshot(probeId: explicitProbeId).streamNextPolls == 1)
}

private func testOrdinaryFutureCancellation() async throws {
    let probeId = "swift-pending-ordinary-future"
    resetProbe(probeId: probeId)
    let operation = Task { () throws -> UInt32 in
        try await pendingOperation(probeId: probeId)
    }
    await waitUntil("pending ordinary future poll") {
        let snapshot = probeSnapshot(probeId: probeId)
        return snapshot.futureStarts == 1 && snapshot.futurePolls >= 1
    }

    operation.cancel()
    operation.cancel()
    do {
        _ = try await operation.value
        fatalError("cancelled ordinary future unexpectedly succeeded")
    } catch is CancellationError {
        // Expected: Rust CALL_CANCELLED becomes CancellationError.
    }
    await waitUntil("pending ordinary future drop") {
        probeSnapshot(probeId: probeId).futureDrops == 1
    }
    let cancelled = probeSnapshot(probeId: probeId)
    assert(cancelled.futureStarts == 1)
    assert(cancelled.futurePolls >= 1)
    assert(cancelled.futureCancelledDrops == 1)
    assert(cancelled.futureDrops == 1)
}

let completion = DispatchGroup()
completion.enter()
Task {
    do {
        try await testCountingStreamIsLazyAndPullBased()
        try await testOptionalAndTypedErrorStreamSteps()
        try await testSingleUseAndIdleCleanup()
        try await testActiveCleanupAndPendingCancellation()
        try await testOrdinaryFutureCancellation()
        completion.leave()
    } catch {
        fatalError("Swift output-stream lifecycle fixture failed: \(error)")
    }
}
completion.wait()
