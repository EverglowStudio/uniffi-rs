import Foundation

fileprivate struct RustBuffer {
    var value: Int = 0
    static func empty() -> Self { Self() }
}
fileprivate struct RustCallStatus {
    var code: Int8
    var errorBuf: RustBuffer
    init() { code = 0; errorBuf = RustBuffer() }
    init(code: Int8, errorBuf: RustBuffer) {
        self.code = code
        self.errorBuf = errorBuf
    }
}
fileprivate let CALL_ERROR: Int8 = 1
fileprivate let CALL_UNEXPECTED_ERROR: Int8 = 2
fileprivate func lowerString(_ value: String) -> RustBuffer { RustBuffer() }
fileprivate struct UniffiForeignFutureResultRustBuffer {
    var returnValue: RustBuffer
    var callStatus: RustCallStatus
}
fileprivate struct UniffiForeignFutureResultVoid { var callStatus: RustCallStatus }
// Model capture of C function pointers in explicitly Sendable Task closures.
fileprivate typealias UniffiForeignFutureCompleteRustBuffer = @Sendable (UInt64, UniffiForeignFutureResultRustBuffer) -> Void
fileprivate typealias UniffiForeignFutureCompleteVoid = @Sendable (UInt64, UniffiForeignFutureResultVoid) -> Void
fileprivate struct UniffiForeignFutureDroppedCallbackStruct {
    var handle: UInt64 = 0
    var free: (UInt64) -> Void = { _ in }
}
fileprivate enum UniffiInternalError: Error { case unexpectedStaleHandle, streamConsumed, concurrentStreamNext }

fileprivate final class Signal: @unchecked Sendable {
    private let lock = NSLock()
    private var signalled = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    var isSet: Bool { lock.withLock { signalled } }
    func fire() {
        let pending = lock.withLock {
            signalled = true
            let pending = waiters
            waiters.removeAll()
            return pending
        }
        for waiter in pending { waiter.resume() }
    }
    func wait() async {
        await withCheckedContinuation { continuation in
            let ready = lock.withLock {
                if signalled { return true }
                waiters.append(continuation)
                return false
            }
            if ready { continuation.resume() }
        }
    }
}
fileprivate final class Probe: @unchecked Sendable {
    let entered = Signal()
    let cancelled = Signal()
    let release = Signal()
    let destroyed = Signal()
    let deinitEntered = Signal()
    let deinitRelease = DispatchSemaphore(value: 0)
    var blockDeinit = false
    var immediate = false
    var fails = false
}
fileprivate struct Sequence: AsyncSequence, Sendable {
    typealias Element = Int
    let probe: Probe
    func makeAsyncIterator() -> Iterator { Iterator(probe) }
    final class Iterator: AsyncIteratorProtocol {
        let probe: Probe
        init(_ probe: Probe) { self.probe = probe }
        func next() async throws -> Int? {
            probe.entered.fire()
            if probe.immediate { return 7 }
            let probe = probe
            await withTaskCancellationHandler {
                await probe.release.wait()
            } onCancel: { probe.cancelled.fire() }
            if probe.fails { throw UniffiInternalError.unexpectedStaleHandle }
            return nil
        }
        deinit {
            probe.deinitEntered.fire()
            if probe.blockDeinit { probe.deinitRelease.wait() }
            probe.destroyed.fire()
        }
    }
}
fileprivate func create(_ probe: Probe) -> UInt64 {
    uniffiCreateInputStream(Sequence(probe: probe), lowerNext: { RustBuffer(value: $0 ?? -1) }, lowerError: { _ in RustBuffer() })
}
fileprivate func close(_ handle: UInt64, _ probe: Probe, _ ack: Signal) -> UniffiForeignFutureDroppedCallbackStruct {
    var dropped = UniffiForeignFutureDroppedCallbackStruct()
    uniffiInputStreamCloseCallback(handle: handle, callback: { _, result in
        precondition(result.callStatus.code == 0)
        precondition(probe.destroyed.isSet, "close ack preceded iterator deinit")
        ack.fire()
    }, callbackData: 0, droppedCallback: &dropped)
    return dropped
}
fileprivate func next(_ handle: UInt64, _ callback: @escaping UniffiForeignFutureCompleteRustBuffer) -> UniffiForeignFutureDroppedCallbackStruct {
    var dropped = UniffiForeignFutureDroppedCallbackStruct()
    uniffiInputStreamNextCallback(handle: handle, callback: callback, callbackData: 0, droppedCallback: &dropped)
    return dropped
}

fileprivate typealias UniffiRustFutureContinuationCallback = @Sendable (UInt64, Int8) -> Void
private func uniffiEnsureGateInitialized() {}
private let CALL_CANCELLED: Int8 = 3
private func makeRustCall<T>(_ callback: (UnsafeMutablePointer<RustCallStatus>) -> T, errorHandler: ((RustBuffer) throws -> Error)?) throws -> T {
    var status = RustCallStatus()
    let value = callback(&status)
    if status.code == CALL_CANCELLED { throw CancellationError() }
    if status.code != 0 { throw UniffiInternalError.unexpectedStaleHandle }
    return value
}

private let UNIFFI_RUST_FUTURE_POLL_READY: Int8 = 0

fileprivate final class StreamSteps<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [__UniffiStreamNext<T>]
    private var pulls = 0
    init(_ values: [__UniffiStreamNext<T>]) { self.values = values }
    var count: Int { lock.withLock { pulls } }
    func next() -> __UniffiStreamNext<T> {
        lock.withLock { pulls += 1; return values.removeFirst() }
    }
}
fileprivate final class FutureProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var callback: UniffiRustFutureContinuationCallback?
    private var data: UInt64 = 0
    private var polls = 0
    private var frees = 0
    private var cancels = 0
    let entered = Signal()
    let repolled = Signal()
    var freeCount: Int { lock.withLock { frees } }
    var cancelCount: Int { lock.withLock { cancels } }
    func poll(_ callback: @escaping UniffiRustFutureContinuationCallback, _ data: UInt64) {
        let second = lock.withLock {
            self.callback = callback
            self.data = data
            polls += 1
            return polls > 1
        }
        entered.fire()
        if second { repolled.fire() }
    }
    func signal(_ result: Int8) {
        let (callback, data) = lock.withLock {
            let callback = self.callback
            self.callback = nil
            return (callback, self.data)
        }
        callback?(data, result)
    }
    func cancel() { lock.withLock { cancels += 1 }; signal(1) }
    func free() { lock.withLock { frees += 1 } }
}

fileprivate func consumeOne(_ stream: UniFfiStream<Int>) async throws {
    for try await value in stream { precondition(value == 1); break }
}

fileprivate final class HostValue: Sendable {
    let released: Signal
    init(_ released: Signal) { self.released = released }
    deinit { released.fire() }
}
