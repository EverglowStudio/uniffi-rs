/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import Foundation
import uniffi_bindgen_tests

func equal<T: Equatable>(_ actual: T, _ expected: T, _ message: String = "") {
    precondition(actual == expected, "\(message): \(actual) != \(expected)")
}

func eventually(_ message: String, _ predicate: @Sendable () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(10)
    while !predicate() {
        precondition(ContinuousClock.now < deadline, message)
        try await Task.sleep(for: .milliseconds(1))
    }
}

func staysPending(_ finished: TestLocked<Bool>) async throws {
    for _ in 0..<20 {
        await Task.yield()
        try await Task.sleep(for: .milliseconds(1))
        precondition(!finished.read(), "acknowledgement preceded the cleanup gate")
    }
}

func cancelled<T: Sendable>(_ task: Task<T, Error>) async {
    do {
        _ = try await task.value
        preconditionFailure("expected CancellationError")
    } catch is CancellationError {
    } catch {
        preconditionFailure("unexpected cancellation result: \(error)")
    }
}

func counts(_ tracker: UnifiedTracker, _ started: UInt32, _ polls: UInt32, _ dropped: UInt32, _ cleaned: UInt32) {
    let value = tracker.counts()
    equal(value.started, started, "started")
    equal(value.polls, polls, "polls")
    equal(value.dropped, dropped, "dropped")
    equal(value.cleaned, cleaned, "cleaned")
}

// These gates deliberately ignore Task cancellation. Only the test opens them,
// so a cancellation request cannot stand in for iterator/frame destruction.
final class SwiftGate: @unchecked Sendable {
    private let lock = NSLock()
    private var opened = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    var isOpen: Bool { lock.withLock { opened } }

    func open() {
        let ready = lock.withLock {
            opened = true
            let ready = waiters
            waiters = []
            return ready
        }
        for waiter in ready { waiter.resume() }
    }

    func wait() async {
        await withCheckedContinuation { continuation in
            let resume = lock.withLock {
                if opened { return true }
                waiters.append(continuation)
                return false
            }
            if resume { continuation.resume() }
        }
    }
}

struct SourceCounts: Sendable {
    var created = 0
    var pulls = 0
    var frames = 0
    var dropped = 0
}

final class SourceProbe: Sendable {
    let counts = TestLocked(SourceCounts())
    let started = SwiftGate()
    let cancelled = SwiftGate()
    let dropStarted = SwiftGate()
    let dropRelease: DispatchSemaphore?

    init(blockDrop: Bool = false) {
        dropRelease = blockDrop ? DispatchSemaphore(value: 0) : nil
    }
}

struct SwiftValues: AsyncSequence, Sendable {
    typealias Element = UInt32
    let count: UInt32
    let probe: SourceProbe
    var body: SwiftGate? = nil
    var failAt: UInt32? = nil

    func makeAsyncIterator() -> Iterator { Iterator(self) }

    // The runtime must serialize this deliberately non-Sendable iterator.
    final class Iterator: AsyncIteratorProtocol {
        let source: SwiftValues
        var index: UInt32 = 0

        init(_ source: SwiftValues) {
            self.source = source
            source.probe.counts.update { $0.created += 1 }
        }

        deinit {
            source.probe.dropStarted.open()
            source.probe.dropRelease?.wait()
            source.probe.counts.update { $0.dropped += 1 }
        }

        func next() async throws -> UInt32? {
            let probe = source.probe
            probe.counts.update { $0.pulls += 1; $0.frames += 1 }
            defer { probe.counts.update { $0.frames -= 1 } }
            probe.started.open()
            if let body = source.body, index == 0 {
                await withTaskCancellationHandler {
                    await body.wait()
                } onCancel: {
                    probe.cancelled.open()
                }
            }
            try Task.checkCancellation()
            if source.failAt == index { throw UnifiedError.Item }
            guard index < source.count else { return nil }
            defer { index += 1 }
            return index
        }
    }
}

func drain<S: AsyncSequence>(_ source: S) async throws -> [S.Element] {
    var values: [S.Element] = []
    for try await value in source { values.append(value) }
    return values
}

func testFutures() async throws {
    equal(try await unifiedReady(value: 11), 11)
    equal(try await unifiedFuture(value: 12), 12)
    equal(try await unifiedBoxed(value: 13), 13)
    equal(unifiedInfallible(value: 14), 14)
    try await unifiedInfallibleUnit()

    for shouldCancel in [false, true] {
        let tracker = UnifiedTracker()
        let body = UnifiedGate()
        let cleanup = UnifiedGate()
        let finished = TestLocked(false)
        let task = Task {
            defer { finished.update { $0 = true } }
            return try await unifiedPending(tracker: tracker, body: body, cleanup: cleanup)
        }
        try await eventually("pending future did not start") { tracker.counts().started == 1 }
        counts(tracker, 1, 0, 0, 0)
        if shouldCancel { task.cancel() } else { body.open() }
        try await eventually("future frame was not dropped") { tracker.counts().dropped == 1 }
        counts(tracker, 1, 0, 1, 0)
        try await staysPending(finished)
        cleanup.open()
        if shouldCancel { await cancelled(task) } else { equal(try await task.value, 7) }
        counts(tracker, 1, 0, 1, 1)
    }
    print("PASS Future ready/impl/boxed/Infallible and gated success/cancel cleanup")
}

func testDefaultRuntime() async throws {
    let tracker = UnifiedTracker()
    let cleanup = UnifiedGate()
    let finished = TestLocked(false)
    let task = Task {
        defer {
            // cleaned is set after the Rust cleanup gate and its Tokio timer.
            // Check in this Task before it settles, not only after joining it.
            equal(tracker.counts().cleaned, 1, "default runtime returned before cleanup timer")
            finished.update { $0 = true }
        }
        return try await unifiedRuntimeReady(tracker: tracker, cleanup: cleanup)
    }
    try await eventually("default-runtime future was not dropped") { tracker.counts().dropped == 1 }
    counts(tracker, 1, 0, 1, 0)
    try await staysPending(finished)
    cleanup.open()
    equal(try await task.value, 47)
    counts(tracker, 1, 0, 1, 1)

    // The exported impl is in runtime_placement, separate from the Rust type.
    let service = UnifiedService()
    equal(try await service.runtimeActive(), true)
    print("PASS default runtime success waits cleanup timer and cross-module method enters Tokio")
}

func testTokioFutures() async throws {
    for shouldCancel in [false, true] {
        let tracker = UnifiedTracker()
        let body = UnifiedGate()
        let cleanup = UnifiedGate()
        let finished = TestLocked(false)
        let task = Task {
            defer {
                equal(tracker.counts().cleaned, 1, "explicit Tokio future settled before cleanup timer")
                finished.update { $0 = true }
            }
            return try await unifiedTokioPending(tracker: tracker, body: body, cleanup: cleanup)
        }
        try await eventually("explicit Tokio future did not start") { tracker.counts().started == 1 }
        counts(tracker, 1, 0, 0, 0)
        if shouldCancel { task.cancel() } else { body.open() }
        try await eventually("explicit Tokio future was not dropped") { tracker.counts().dropped == 1 }
        counts(tracker, 1, 0, 1, 0)
        try await staysPending(finished)
        cleanup.open()
        if shouldCancel { await cancelled(task) } else { equal(try await task.value, 47) }
        counts(tracker, 1, 0, 1, 1)
    }
    print("PASS explicit Tokio success/cancellation wait Drop, cleanup gate and cleanup timer")
}

func testTokioStreamClose() async throws {
    for pullBeforeClose in [false, true] {
        let tracker = UnifiedTracker()
        let cleanup = UnifiedGate()
        // Synchronous factory: constructing or closing an unpolled stream must
        // not require an ambient Tokio context on the Swift caller's thread.
        let stream = unifiedTokioValues(tracker: tracker, cleanup: cleanup)
        counts(tracker, 0, 0, 0, 0)
        var iterator = stream.makeAsyncIterator()
        if pullBeforeClose { equal(try await iterator.next(), 47) }
        let started: UInt32 = pullBeforeClose ? 1 : 0
        let polls = tracker.counts().polls
        if pullBeforeClose { precondition(polls > 0, "Tokio stream did not poll") }
        counts(tracker, started, polls, 0, 0)
        try await Task.sleep(for: .milliseconds(20))
        counts(tracker, started, polls, 0, 0)

        let finished = TestLocked(false)
        let close = Task {
            defer {
                equal(tracker.counts().cleaned, 1, "Tokio stream close settled before cleanup timer")
                finished.update { $0 = true }
            }
            try await stream.close()
        }
        try await eventually("Tokio stream close did not drop its frame") { tracker.counts().dropped == 1 }
        counts(tracker, started, polls, 1, 0)
        try await staysPending(finished)
        cleanup.open()
        try await close.value
        counts(tracker, started, polls, 1, 1)
        equal(try await iterator.next(), nil)
        try await stream.close()
        counts(tracker, started, polls, 1, 1)
    }
    print("PASS synchronous Tokio stream factory, poll and unpolled/polled close await cleanup timer")
}

func testOutputValues() async throws {
    let tracker = UnifiedTracker()
    let cleanup = UnifiedGate()
    let stream = unifiedValues(count: 2, fail: false, tracker: tracker, cleanup: cleanup)
    counts(tracker, 1, 0, 0, 0)
    var iterator = stream.makeAsyncIterator()
    let first = try await iterator.next()
    guard case .some(.none) = first else { preconditionFailure("Item(None) became EOF") }
    counts(tracker, 1, 1, 0, 0)
    try await Task.sleep(for: .milliseconds(20))
    counts(tracker, 1, 1, 0, 0)
    equal(try await iterator.next(), .some(.some(1)))
    let finished = TestLocked(false)
    let eof = Task { [iterator] in
        var iterator = iterator
        defer { finished.update { $0 = true } }
        return try await iterator.next()
    }
    try await eventually("EOF did not drop the Rust stream") { tracker.counts().dropped == 1 }
    counts(tracker, 1, 3, 1, 0)
    try await staysPending(finished)
    cleanup.open()
    equal(try await eof.value, nil)
    try await stream.close()
    try await stream.close()
    counts(tracker, 1, 3, 1, 1)
    equal(try await drain(unifiedPlainValues(count: 3)), [0, 1, 2])
    equal(try await drain(unifiedCanonicalValues(count: 3)), [0, 1, 2])
    print("PASS optional Item/EOF, no prefetch, infallible streams and repeated close")
}

func testOutputError() async throws {
    let tracker = UnifiedTracker()
    let cleanup = UnifiedGate()
    let stream = unifiedValues(count: 5, fail: true, tracker: tracker, cleanup: cleanup)
    var iterator = stream.makeAsyncIterator()
    _ = try await iterator.next()
    let finished = TestLocked(false)
    let failure = Task { [iterator] in
        var iterator = iterator
        defer { finished.update { $0 = true } }
        return try await iterator.next()
    }
    try await eventually("business error did not drop the stream") { tracker.counts().dropped == 1 }
    counts(tracker, 1, 2, 1, 0)
    try await staysPending(finished)
    cleanup.open()
    do {
        _ = try await failure.value
        preconditionFailure("expected first business item error")
    } catch UnifiedError.Item {}
    equal(try await iterator.next(), nil)
    counts(tracker, 1, 2, 1, 1)
    print("PASS first item error terminates after cleanup")
}

func testOutputClose() async throws {
    let tracker = UnifiedTracker()
    let cleanup = UnifiedGate()
    let stream = unifiedValues(count: 5, fail: false, tracker: tracker, cleanup: cleanup)
    let finished = TestLocked(false)
    let first = Task {
        defer { finished.update { $0 = true } }
        try await stream.close()
    }
    try await eventually("close did not drop the stream") { tracker.counts().dropped == 1 }
    first.cancel()
    let second = Task { try await stream.close() }
    try await staysPending(finished)
    counts(tracker, 1, 0, 1, 0)
    cleanup.open()
    try await first.value
    try await second.value
    counts(tracker, 1, 0, 1, 1)

    let earlyTracker = UnifiedTracker()
    let earlyCleanup = UnifiedGate()
    let early = unifiedValues(count: 5, fail: false, tracker: earlyTracker, cleanup: earlyCleanup)
    for try await _ in early { break }
    try await eventually("loop break did not request cleanup") { earlyTracker.counts().dropped == 1 }
    counts(earlyTracker, 1, 1, 1, 0)
    let earlyFinished = TestLocked(false)
    let earlyClose = Task {
        defer { earlyFinished.update { $0 = true } }
        try await early.close()
    }
    try await staysPending(earlyFinished)
    earlyCleanup.open()
    try await earlyClose.value
    counts(earlyTracker, 1, 1, 1, 1)
    print("PASS dropped close waiter, shared ack and early-loop cleanup")
}

func testFactoriesAndMethods() async throws {
    let service = UnifiedService()
    equal(try await service.future(value: 17), 17)
    equal(try await drain(service.values(count: 3)), [0, 1, 2])
    let created = try await UnifiedService.create()
    equal(try await drain(try await created.valuesAsync(count: 2)), [0, 1])

    for fail in [false, true] {
        let tracker = UnifiedTracker()
        let body = UnifiedGate()
        let cleanup = UnifiedGate()
        let finished = TestLocked(false)
        let factory = Task {
            defer { finished.update { $0 = true } }
            return try await unifiedValuesFactory(count: 2, fail: fail, body: body, tracker: tracker, cleanup: cleanup)
        }
        try await staysPending(finished)
        counts(tracker, 0, 0, 0, 0)
        body.open()
        if fail {
            do {
                _ = try await factory.value
                preconditionFailure("expected factory error")
            } catch UnifiedError.Factory {}
            counts(tracker, 0, 0, 0, 0)
        } else {
            let stream = try await factory.value
            counts(tracker, 1, 0, 0, 0)
            // Cancellation of the settled factory does not own the subscription.
            factory.cancel()
            var iterator = stream.makeAsyncIterator()
            let item = try await iterator.next()
            guard case .some(.none) = item else { preconditionFailure("factory lost its subscription") }
            cleanup.open()
            try await stream.close()
            counts(tracker, 1, 1, 1, 1)
        }
    }
    let tracker = UnifiedTracker()
    let body = UnifiedGate()
    let cleanup = UnifiedGate()
    let factory = Task { try await unifiedValuesFactory(count: 2, fail: false, body: body, tracker: tracker, cleanup: cleanup) }
    factory.cancel()
    await cancelled(factory)
    counts(tracker, 0, 0, 0, 0)
    print("PASS shared methods, sync/async factories, factory error and distinct subscription ownership")
}

func testInputs() async throws {
    let service = UnifiedService()
    let sumProbe = SourceProbe()
    equal(try await service.sum(source: SwiftValues(count: 4, probe: sumProbe)), 6)
    equal(sumProbe.counts.read().pulls, 5)
    equal(sumProbe.counts.read().dropped, 1)

    for asyncFactory in [false, true] {
        let probe = SourceProbe()
        let source = SwiftValues(count: 3, probe: probe)
        let stream = asyncFactory ? try await service.echoAsync(source: source) : service.echo(source: source)
        equal(probe.counts.read().pulls, 0)
        equal(probe.counts.read().dropped, 0)
        var iterator = stream.makeAsyncIterator()
        equal(try await iterator.next(), 0)
        equal(probe.counts.read().pulls, 1)
        equal(probe.counts.read().dropped, 0)
        try await stream.close()
        equal(probe.counts.read().dropped, 1)
        equal(probe.counts.read().frames, 0)
    }

    let errorProbe = SourceProbe()
    do {
        _ = try await service.sum(source: SwiftValues(count: 3, probe: errorProbe, failAt: 1))
        preconditionFailure("expected input business error")
    } catch UnifiedError.Item {}
    equal(errorProbe.counts.read().dropped, 1)
    equal(errorProbe.counts.read().pulls, 2)
    print("PASS input consumption/error and sync/async input-to-output ownership")
}

func testInputCancellation() async throws {
    let service = UnifiedService()
    let body = SwiftGate()
    let probe = SourceProbe()
    let finished = TestLocked(false)
    let task = Task {
        defer { finished.update { $0 = true } }
        return try await service.sum(source: SwiftValues(count: 3, probe: probe, body: body))
    }
    try await eventually("input next did not start") { probe.started.isOpen }
    task.cancel()
    try await eventually("input next did not receive cancel") { probe.cancelled.isOpen }
    try await staysPending(finished)
    equal(probe.counts.read().dropped, 0)
    body.open()
    await cancelled(task)
    equal(probe.counts.read().frames, 0)
    equal(probe.counts.read().dropped, 1)

    let outputBody = SwiftGate()
    let outputProbe = SourceProbe()
    let stream = service.echo(source: SwiftValues(count: 2, probe: outputProbe, body: outputBody))
    let iterator = stream.makeAsyncIterator()
    let outputFinished = TestLocked(false)
    let next = Task { [iterator] in
        var iterator = iterator
        defer { outputFinished.update { $0 = true } }
        return try await iterator.next()
    }
    try await eventually("echo next did not start") { outputProbe.started.isOpen }
    var concurrent = iterator
    do {
        _ = try await concurrent.next()
        preconditionFailure("concurrent next must be rejected")
    } catch is CancellationError {
        preconditionFailure("concurrent rejection must not cancel the accepted pull")
    } catch {}
    equal(outputProbe.counts.read().pulls, 1)
    next.cancel()
    try await eventually("echo producer did not receive cancel") { outputProbe.cancelled.isOpen }
    try await staysPending(outputFinished)
    outputBody.open()
    await cancelled(next)
    try await stream.close()
    equal(outputProbe.counts.read().frames, 0)
    equal(outputProbe.counts.read().dropped, 1)
    print("PASS input/echo Task cancellation waits real frames and rejects concurrent next")
}

final class CallbackProbe: Sendable {
    let started = SwiftGate()
    let cancelled = SwiftGate()
    let frames = TestLocked(0)
}

final class CallbackFrame {
    let probe: CallbackProbe
    init(_ probe: CallbackProbe) { self.probe = probe; probe.frames.update { $0 += 1 } }
    deinit { probe.frames.update { $0 -= 1 } }
}

final class SwiftHost: UnifiedHost {
    let sourceProbe: SourceProbe
    let callbackProbe = CallbackProbe()
    let body: SwiftGate?

    init(body: SwiftGate? = nil, sourceProbe: SourceProbe = SourceProbe()) {
        self.body = body
        self.sourceProbe = sourceProbe
    }

    func value(value: UInt32) async throws -> UInt32 {
        let frame = CallbackFrame(callbackProbe)
        defer { withExtendedLifetime(frame) {} }
        let result = try await unifiedReady(value: value) // Rust -> Swift -> Rust reentry.
        callbackProbe.started.open()
        if let body {
            await withTaskCancellationHandler {
                await body.wait()
            } onCancel: {
                self.callbackProbe.cancelled.open()
            }
        }
        return result
    }

    func boxed(value: UInt32) async throws -> UInt32 { try await self.value(value: value) }

    func values(count: UInt32) -> UniFfiStream<UInt32> {
        UniFfiStream(SwiftValues(count: count, probe: sourceProbe))
    }

    func produce(count: UInt32) async throws -> UniFfiStream<UInt32> {
        let frame = CallbackFrame(callbackProbe)
        defer { withExtendedLifetime(frame) {} }
        callbackProbe.started.open()
        if let body {
            await withTaskCancellationHandler {
                await body.wait()
            } onCancel: {
                self.callbackProbe.cancelled.open()
            }
        }
        if count == 99 { throw UnifiedError.Factory }
        return values(count: count)
    }

    func consume<S0: AsyncSequence & Sendable>(source: S0) async throws -> UInt32 where S0.Element == UInt32 {
        var sum: UInt32 = 0
        for try await value in source { sum += value }
        return sum
    }

    func echo<S0: AsyncSequence & Sendable>(source: S0) -> UniFfiStream<UInt32> where S0.Element == UInt32 {
        UniFfiStream(source)
    }

    func echoAsync<S0: AsyncSequence & Sendable>(source: S0) async throws -> UniFfiStream<UInt32> where S0.Element == UInt32 {
        UniFfiStream(source)
    }
}

func testHostCallbacks() async throws {
    let host = SwiftHost()
    equal(try await unifiedHostValue(host: host, value: 21), 21)
    equal(try await unifiedHostBoxed(host: host, value: 22), 22)
    equal(try await unifiedHostValues(host: host, count: 4), 6)
    equal(host.sourceProbe.counts.read().dropped, 1)
    equal(try await unifiedHostFactory(host: host, count: 3), 3)
    equal(host.sourceProbe.counts.read().dropped, 2)
    do {
        _ = try await unifiedHostFactory(host: host, count: 99)
        preconditionFailure("expected host factory error")
    } catch UnifiedError.Factory {}
    let sourceProbe = SourceProbe()
    equal(try await unifiedHostConsume(host: host, source: SwiftValues(count: 4, probe: sourceProbe)), 6)
    equal(sourceProbe.counts.read().dropped, 1)
    equal(host.callbackProbe.frames.read(), 0)

    for boxed in [false, true] {
        let body = SwiftGate()
        let delayed = SwiftHost(body: body)
        let finished = TestLocked(false)
        let task = Task {
            defer { finished.update { $0 = true } }
            return boxed
                ? try await unifiedHostBoxed(host: delayed, value: 23)
                : try await unifiedHostValue(host: delayed, value: 23)
        }
        try await eventually("reverse callback did not start") { delayed.callbackProbe.started.isOpen }
        task.cancel()
        try await eventually("dropped callback did not request host cancellation") { delayed.callbackProbe.cancelled.isOpen }
        equal(delayed.callbackProbe.frames.read(), 1)
        try await staysPending(finished)
        body.open()
        await cancelled(task)
        equal(delayed.callbackProbe.frames.read(), 0)
    }
    let factoryBody = SwiftGate()
    let unclaimed = SourceProbe(blockDrop: true)
    let factoryHost = SwiftHost(body: factoryBody, sourceProbe: unclaimed)
    let factoryFinished = TestLocked(false)
    let factory = Task {
        defer { factoryFinished.update { $0 = true } }
        return try await unifiedHostFactory(host: factoryHost, count: 3)
    }
    try await eventually("host factory did not start") { factoryHost.callbackProbe.started.isOpen }
    factory.cancel()
    try await eventually("host factory did not receive cancellation") { factoryHost.callbackProbe.cancelled.isOpen }
    factoryBody.open()
    try await eventually("unclaimed host subscription did not start closing") { unclaimed.dropStarted.isOpen }
    try await staysPending(factoryFinished)
    unclaimed.dropRelease?.signal()
    await cancelled(factory)
    equal(unclaimed.counts.read().dropped, 1)
    equal(unclaimed.counts.read().pulls, 0)
    print("PASS host trait async/boxed/streams/factory/input/reentry and post-frame cancellation ack")
}

func echoLifetime(_ factory: @Sendable (SwiftValues) async throws -> UniFfiStream<UInt32>) async throws {
    let probe = SourceProbe(blockDrop: true)
    let stream = try await factory(SwiftValues(count: 3, probe: probe))
    equal(probe.counts.read().created, 1)
    equal(probe.counts.read().pulls, 0)
    equal(probe.counts.read().dropped, 0)
    precondition(!probe.dropStarted.isOpen, "factory released an input moved into its output")
    var iterator = stream.makeAsyncIterator()
    equal(try await iterator.next(), 0)
    equal(probe.counts.read().pulls, 1)
    precondition(!probe.dropStarted.isOpen)
    let finished = TestLocked(false)
    let close = Task {
        defer { finished.update { $0 = true } }
        try await stream.close()
    }
    try await eventually("echo close did not release its input iterator") { probe.dropStarted.isOpen }
    try await staysPending(finished)
    equal(probe.counts.read().dropped, 0)
    probe.dropRelease?.signal()
    try await close.value
    try await stream.close()
    equal(probe.counts.read().dropped, 1)
    equal(probe.counts.read().frames, 0)
    equal(probe.counts.read().pulls, 1)
}

func testRustTraitWrapper() async throws {
    let host = unifiedRustHost()
    precondition(host is UnifiedHostImpl, "expected the generated Rust trait proxy")
    equal(try await host.value(value: 31), 31)
    equal(try await host.boxed(value: 32), 32)
    equal(try await drain(host.values(count: 4)), [0, 1, 2, 3])
    equal(try await drain(try await host.produce(count: 3)), [0, 1, 2])
    do {
        _ = try await host.produce(count: 99)
        preconditionFailure("expected Rust trait factory error")
    } catch UnifiedError.Factory {}
    let consumed = SourceProbe()
    equal(try await host.consume(source: SwiftValues(count: 4, probe: consumed)), 6)
    equal(consumed.counts.read().dropped, 1)
    try await echoLifetime { host.echo(source: $0) }
    try await echoLifetime { try await host.echoAsync(source: $0) }
    print("PASS Rust-returned trait proxy Future/boxed/stream/factory/input and sync/async echo")
}

func testReverseEcho() async throws {
    let host = SwiftHost()
    try await echoLifetime { unifiedHostEcho(host: host, source: $0) }
    try await echoLifetime { try await unifiedHostEchoAsync(host: host, source: $0) }
    print("PASS reverse sync/async echo keeps input alive and awaits its actual iterator destruction")
}

func testLocalOwner() async throws {
    let owner = try UnifiedOwner()
    let local = try await UnifiedLocal.create(owner: owner)
    equal(try await local.increment(), 1)
    equal(try await local.increment(), 2)
    equal(try await drain(try await local.values(count: 4)), [0, 1, 2, 3])
    try await local.close()
    try await local.close()
    do {
        _ = try await local.increment()
        preconditionFailure("closed local proxy accepted a call")
    } catch {}
    try await owner.close()
    try await owner.close()
    print("PASS local construction/method/!Send stream/drop owner assertions and repeatable close")
}

func testLocalCancellation() async throws {
    let owner = try UnifiedOwner()
    let local = try await UnifiedLocal.create(owner: owner)
    let tracker = UnifiedTracker()
    let body = UnifiedGate()
    let cleanup = UnifiedGate()
    let finished = TestLocked(false)
    let pending = Task {
        defer { finished.update { $0 = true } }
        return try await local.pending(tracker: tracker, body: body, cleanup: cleanup)
    }
    try await eventually("local method did not start on its owner") { tracker.counts().started == 1 }
    pending.cancel()
    try await eventually("local method was not dropped on cancellation") { tracker.counts().dropped == 1 }
    counts(tracker, 1, 0, 1, 0)
    try await staysPending(finished)
    cleanup.open()
    await cancelled(pending)
    counts(tracker, 1, 0, 1, 1)
    equal(try await local.increment(), 1)
    try await local.close()
    try await owner.close()

    for closeOwner in [false, true] {
        let streamOwner = try UnifiedOwner()
        let streamLocal = try await UnifiedLocal.create(owner: streamOwner)
        let streamTracker = UnifiedTracker()
        let streamBody = UnifiedGate()
        let streamCleanup = UnifiedGate()
        let stream = try await streamLocal.pendingValues(tracker: streamTracker, body: streamBody, cleanup: streamCleanup)
        counts(streamTracker, 1, 0, 0, 0)
        let nextFinished = TestLocked(false)
        let next = Task {
            var iterator = stream.makeAsyncIterator()
            defer { nextFinished.update { $0 = true } }
            return try await iterator.next()
        }
        try await eventually("local !Send stream was not polled on owner") { streamTracker.counts().polls > 0 }
        let ownerFinished = TestLocked(false)
        let ownerClose: Task<Void, Error>?
        if closeOwner {
            ownerClose = Task {
                defer { ownerFinished.update { $0 = true } }
                try await streamOwner.close()
            }
        } else {
            ownerClose = nil
            next.cancel()
        }
        try await eventually("local !Send stream was not dropped") { streamTracker.counts().dropped == 1 }
        let polls = streamTracker.counts().polls
        counts(streamTracker, 1, polls, 1, 0)
        try await staysPending(nextFinished)
        if closeOwner { try await staysPending(ownerFinished) }
        let close: Task<Void, Error>?
        if closeOwner {
            // Let owner.close alone terminate next; a stream.close here would
            // introduce a second cancellation path and mask owner propagation.
            close = nil
        } else {
            let closeFinished = TestLocked(false)
            close = Task {
                defer { closeFinished.update { $0 = true } }
                try await stream.close()
            }
            try await staysPending(closeFinished)
        }
        streamCleanup.open()
        await cancelled(next)
        try await close?.value
        try await ownerClose?.value
        counts(streamTracker, 1, polls, 1, 1)
        try await stream.close()
        try await streamLocal.close()
        try await streamOwner.close()
    }
    print("PASS local Task cancellation and owner.close wait !Send method/stream cleanup gates")
}

func testResourceLifecycle() async throws {
    let tracker = UnifiedTracker()
    let cleanup = UnifiedGate()
    let resource = UnifiedLifecycle(tracker: tracker, cleanup: cleanup, fail: false)
    equal(try resource.syncPing(), 5)
    equal(resource.plainPing(), 3)
    let otherTracker = UnifiedTracker()
    let openGate = UnifiedGate()
    openGate.open()
    let other = UnifiedLifecycle(tracker: otherTracker, cleanup: openGate, fail: false)
    let firstFinished = TestLocked(false)
    let first = Task {
        defer { firstFinished.update { $0 = true } }
        try await resource.close()
    }
    try await eventually("resource close did not start") { tracker.counts().started == 1 }
    let second = Task { try await resource.close() }
    first.cancel()
    try await staysPending(firstFinished)
    do {
        _ = try await resource.ping()
        preconditionFailure("new operation admitted after close started")
    } catch {}
    do {
        _ = try resource.syncPing()
        preconditionFailure("new synchronous operation admitted after close started")
    } catch {}
    equal(try await other.ping(), 7)
    cleanup.open()
    try await first.value
    try await second.value
    try await resource.close()
    withExtendedLifetime(resource) { counts(tracker, 1, 0, 1, 1) }
    equal(otherTracker.counts().dropped, 0)
    try await other.close()
    equal(otherTracker.counts().dropped, 1)

    let liveTracker = UnifiedTracker()
    let live = UnifiedLifecycle(tracker: liveTracker, cleanup: openGate, fail: false)
    let body = UnifiedGate()
    let pending = Task { try await live.pending(body: body) }
    try await eventually("in-flight call did not start") { liveTracker.counts().polls == 1 }
    try await live.close()
    equal(liveTracker.counts().dropped, 0)
    body.open()
    equal(try await pending.value, 9)
    equal(liveTracker.counts().dropped, 1)

    let failedTracker = UnifiedTracker()
    let failed = UnifiedLifecycle(tracker: failedTracker, cleanup: openGate, fail: true)
    let failedOne = Task { try await failed.close() }
    let failedTwo = Task { try await failed.close() }
    for task in [failedOne, failedTwo] {
        do { try await task.value; preconditionFailure("expected close failure") }
        catch UnifiedError.Closed {}
    }
    do { try await failed.close(); preconditionFailure("expected cached close failure") }
    catch UnifiedError.Closed {}
    withExtendedLifetime(failed) { counts(failedTracker, 1, 0, 1, 1) }

    let firstTracker = UnifiedTracker()
    let firstCleanup = UnifiedGate()
    let secondTracker = UnifiedTracker()
    let firstStream = unifiedValues(count: 3, fail: false, tracker: firstTracker, cleanup: firstCleanup)
    let secondStream = unifiedValues(count: 2, fail: false, tracker: secondTracker, cleanup: openGate)
    // Early break only requests cancellation; close is the acknowledgement.
    for try await _ in firstStream { break }
    let streamFinished = TestLocked(false)
    let streamClose = Task {
        defer { streamFinished.update { $0 = true } }
        try await firstStream.close()
    }
    try await eventually("first subscription did not drop") { firstTracker.counts().dropped == 1 }
    try await staysPending(streamFinished)
    equal(try await drain(secondStream), [nil, 1])
    equal(secondTracker.counts().cleaned, 1)
    equal(firstTracker.counts().cleaned, 0)
    firstCleanup.open()
    try await streamClose.value
    equal(firstTracker.counts().cleaned, 1)
    print("PASS resource close releases retained wrapper; concurrent/cancelled waiters, failure and client isolation")
}

let completion = DispatchGroup()
completion.enter()
Task.detached {
    defer { completion.leave() }
    do {
        try await testResourceLifecycle()
        try await testFutures()
        try await testDefaultRuntime()
        try await testTokioFutures()
        try await testTokioStreamClose()
        try await testOutputValues()
        try await testOutputError()
        try await testOutputClose()
        try await testFactoriesAndMethods()
        try await testInputs()
        try await testInputCancellation()
        try await testHostCallbacks()
        try await testRustTraitWrapper()
        try await testReverseEcho()
        try await testLocalOwner()
        try await testLocalCancellation()
        equal(uniffiInputStreamHandleCountUniffiBindgenTests(), 0)
        equal(uniffiForeignFutureHandleCountUniffiBindgenTests(), 0)
        print("S04 Swift shared fixture: 16 groups passed")
    } catch {
        preconditionFailure("S04 Swift shared fixture failed: \(error)")
    }
}
precondition(completion.wait(timeout: .now() + 90) == .success, "S04 Swift shared fixture timed out")
