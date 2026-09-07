private let UNIFFI_RUST_FUTURE_POLL_READY: Int8 = 0
private let UNIFFI_RUST_FUTURE_POLL_WAKE: Int8 = 1

{%- if include_stream_runtime %}
// The Rust output-stream ABI completes one strict tagged StreamStep as a
// RustBuffer. `Item(nil)` is an item payload when Element is Optional; only a
// distinct Done tag completes the stream, and Error keeps the typed payload.
//
// This is internal so stream functions emitted by other component sources in the
// same Swift module can construct the shared UniFfiStream runtime.
enum __UniffiStreamNext<T: Sendable>: Sendable {
    case item(T)
    case done
    case error(Swift.Error)
}
{%- endif %}

{%- if ci.has_stream_fns() %}
private func __uniffiLiftStreamNext<T: Sendable>(
    _ rustBuffer: RustBuffer,
    readItem: (inout (data: Data, offset: Data.Index)) throws -> T,
    readError: (inout (data: Data, offset: Data.Index)) throws -> Swift.Error
) throws -> __UniffiStreamNext<T> {
    defer {
        rustBuffer.deallocate()
    }
    guard rustBuffer.len > 0, rustBuffer.data != nil else {
        throw UniffiInternalError.bufferOverflow
    }
    var reader = createReader(data: Data(rustBuffer: rustBuffer))
    switch try readInt(&reader) as Int8 {
    case 1:
        let item = try readItem(&reader)
        guard !hasRemaining(reader) else {
            throw UniffiInternalError.incompleteData
        }
        return .item(item)
    case 2:
        guard !hasRemaining(reader) else {
            throw UniffiInternalError.incompleteData
        }
        return .done
    case 3:
        let error = try readError(&reader)
        guard !hasRemaining(reader) else {
            throw UniffiInternalError.incompleteData
        }
        return .error(error)
    default:
        throw UniffiInternalError.unexpectedStreamStepTag
    }
}
{%- endif %}

fileprivate let uniffiContinuationHandleMap = UniffiHandleMap<UnsafeContinuation<Int8, Never>>()

// Rust future handles may be touched by a Swift task's cancellation handler while the
// task is polling or completing. Keep cancellation and final release behind one lock so
// that `rust_future_free` is never called twice or before a cancellation handler has
// finished with the handle.
fileprivate final class UniffiRustFutureHandle: @unchecked Sendable {
    private let lock = NSLock()
    private let handle: UInt64
    private let cancelFunc: @Sendable (UInt64) -> ()
    private let freeFunc: @Sendable (UInt64) -> ()
    private var cancelled = false
    private var freed = false

    init(
        handle: UInt64,
        cancelFunc: @escaping @Sendable (UInt64) -> (),
        freeFunc: @escaping @Sendable (UInt64) -> ()
    ) {
        self.handle = handle
        self.cancelFunc = cancelFunc
        self.freeFunc = freeFunc
    }

    func cancel() {
        lock.withLock {
            guard !cancelled && !freed else {
                return
            }
            cancelled = true
            // Keep the FFI call inside the lifecycle lock. `rust_future_cancel`
            // may synchronously wake the awaiting Swift task, whose deferred free
            // can otherwise race ahead and invalidate this handle.
            cancelFunc(handle)
        }
    }

    func free() {
        lock.withLock {
            guard !freed else {
                return
            }
            freed = true
            freeFunc(handle)
        }
    }
}

fileprivate func uniffiRustCallAsync<F, T: Sendable>(
    rustFutureFunc: @Sendable () -> UInt64,
    pollFunc: @Sendable (UInt64, @escaping UniffiRustFutureContinuationCallback, UInt64) -> (),
    cancelFunc: @escaping @Sendable (UInt64) -> (),
    completeFunc: @Sendable (UInt64, UnsafeMutablePointer<RustCallStatus>) -> F,
    freeFunc: @escaping @Sendable (UInt64) -> (),
    liftFunc: @Sendable (F) throws -> T,
    errorHandler: (@Sendable (RustBuffer) throws -> Swift.Error)?
) async throws -> T {
    // Make sure to call the ensure init function since future creation doesn't have a
    // RustCallStatus param, so doesn't use makeRustCall()
    {{ ensure_init_fn_name }}()
    let rustFuture = rustFutureFunc()
    let rustFutureHandle = UniffiRustFutureHandle(
        handle: rustFuture,
        cancelFunc: cancelFunc,
        freeFunc: freeFunc
    )
    return try await withTaskCancellationHandler(operation: {
        defer {
            rustFutureHandle.free()
        }
        var pollResult: Int8
        repeat {
            pollResult = await withUnsafeContinuation {
                pollFunc(
                    rustFuture,
                    { handle, pollResult in
                        uniffiFutureContinuationCallback(handle: handle, pollResult: pollResult)
                    },
                    uniffiContinuationHandleMap.insert(obj: $0)
                )
            }
        } while pollResult != UNIFFI_RUST_FUTURE_POLL_READY

        return try liftFunc(makeRustCall(
            { completeFunc(rustFuture, $0) },
            errorHandler: errorHandler
        ))
    }, onCancel: {
        rustFutureHandle.cancel()
    })
}

{%- if include_stream_runtime %}
// Output streams use a pull-based custom AsyncSequence.
// Shared by Rust stream handles and Swift AsyncSequence producers. Every next is
// owned by the subscription; close joins it before acknowledging cleanup.
protocol UniffiAsyncSequenceClose: Sendable {
    func close() async throws
}

public struct UniFfiStream<Element: Sendable>: AsyncSequence, Sendable, UniffiAsyncSequenceClose {
    public struct Iterator: AsyncIteratorProtocol, Sendable {
        fileprivate let lifetime: UniFfiStreamIteratorLifetime<Element>
        public mutating func next() async throws -> Element? {
            try await lifetime.next()
        }
    }

    fileprivate let state: UniFfiStreamState<Element>

    init(
        handle: UInt64,
        next: @escaping @Sendable (UInt64) async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping @Sendable (UInt64) -> Void,
        close: @escaping @Sendable (UInt64) async throws -> Void
    ) {
        state = UniFfiStreamState(handle: handle, next: next, cancel: cancel, close: close)
    }

    /// Type erase a single-consumer Swift producer without prefetching.
    public init<S: AsyncSequence & Sendable>(_ sequence: S) where S.Element == Element {
        let source = UniFfiSequenceSource(sequence)
        self.init(
            handle: 0,
            next: { _ in try await source.next() },
            cancel: { _ in },
            close: { _ in try await source.close() }
        )
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(lifetime: UniFfiStreamIteratorLifetime(state: state))
    }

    /// Request cancellation. Use close() to await producer and callback cleanup.
    public func cancel() { state.cancel() }

    public func close() async throws { try await state.close() }
}

// Access to the iterator is serialized by UniFfiStreamState, including close.
fileprivate final class UniFfiSequenceSource<S: AsyncSequence & Sendable>: @unchecked Sendable where S.Element: Sendable {
    private var iterator: S.AsyncIterator?
    private let closeSequence: (@Sendable () async throws -> Void)?

    init(_ sequence: S) {
        iterator = sequence.makeAsyncIterator()
        if let sequence = sequence as? any UniffiAsyncSequenceClose {
            closeSequence = { try await sequence.close() }
        } else {
            closeSequence = nil
        }
    }

    func next() async throws -> __UniffiStreamNext<S.Element> {
        guard var iterator = iterator else { return .done }
        self.iterator = nil
        do {
            if let value = try await iterator.next() {
                self.iterator = iterator
                return .item(value)
            }
            return .done
        } catch {
            throw error
        }
    }

    func close() async throws {
        iterator = nil
        try await closeSequence?()
    }
}

fileprivate final class UniFfiStreamIteratorLifetime<Element: Sendable>: Sendable {
    private let state: UniFfiStreamState<Element>
    private let accepted: Bool

    init(state: UniFfiStreamState<Element>) {
        self.state = state
        accepted = state.claimIterator()
    }

    deinit {
        if accepted { state.cancel() }
    }

    func next() async throws -> Element? {
        guard accepted else { throw UniffiInternalError.streamConsumed }
        return try await state.next()
    }
}

fileprivate final class UniFfiStreamState<Element: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private let nextFunc: @Sendable (UInt64) async throws -> __UniffiStreamNext<Element>
    private let cancelFunc: @Sendable (UInt64) -> Void
    private let closeFunc: @Sendable (UInt64) async throws -> Void
    private var iteratorClaimed = false
    private let handle: UInt64
    private var stopped = false
    private var nextTask: Task<__UniffiStreamNext<Element>, Error>?
    private var closeTask: Task<Void, Error>?

    init(
        handle: UInt64,
        next: @escaping @Sendable (UInt64) async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping @Sendable (UInt64) -> Void,
        close: @escaping @Sendable (UInt64) async throws -> Void
    ) {
        self.handle = handle
        nextFunc = next
        cancelFunc = cancel
        closeFunc = close
    }

    deinit { cancel() }

    func claimIterator() -> Bool {
        lock.withLock {
            guard !iteratorClaimed else { return false }
            iteratorClaimed = true
            return true
        }
    }

    func next() async throws -> Element? {
        if Task.isCancelled {
            try await close()
            throw CancellationError()
        }
        let task = try lock.withLock { () throws -> Task<__UniffiStreamNext<Element>, Error>? in
            if stopped { return nil }
            if nextTask != nil { throw UniffiInternalError.concurrentStreamNext }
            let handle = handle
            let next = nextFunc
            let task = Task { try await next(handle) }
            nextTask = task
            return task
        }
        guard let task else {
            try await close()
            return nil
        }
        do {
            let result = try await withTaskCancellationHandler {
                try await task.value
            } onCancel: {
                self.cancel()
            }
            let cancelled = lock.withLock {
                nextTask = nil
                if stopped { return true }
                switch result {
                case .item: break
                case .done, .error: stopped = true
                }
                return false
            }
            if cancelled || Task.isCancelled {
                try await close()
                throw CancellationError()
            }
            switch result {
            case let .item(value): return .some(value)
            case .done:
                try await close()
                return nil
            case let .error(error):
                try await close()
                throw error
            }
        } catch {
            lock.withLock { nextTask = nil; stopped = true }
            try await close()
            throw error
        }
    }

    private func startClose() -> Task<Void, Error> {
        lock.withLock {
            if let task = closeTask { return task }
            stopped = true
            let pending = nextTask
            let handle = handle
            let cancel = cancelFunc
            let close = closeFunc
            // This task owns cleanup independently of any caller's cancellation.
            let task = Task {
                pending?.cancel()
                cancel(handle)
                _ = try? await pending?.value
                try await close(handle)
            }
            closeTask = task
            return task
        }
    }

    func cancel() { _ = startClose() }
    func close() async throws { try await startClose().value }
}
{%- endif %}

// Callback handlers for an async calls.  These are invoked by Rust when the future is ready.  They
// lift the return value or error and resume the suspended function.
fileprivate func uniffiFutureContinuationCallback(handle: UInt64, pollResult: Int8) {
    if let continuation = try? uniffiContinuationHandleMap.remove(handle: handle) {
        continuation.resume(returning: pollResult)
    } else {
        print("uniffiFutureContinuationCallback invalid handle")
    }
}

{%- if ci.has_async_callback_interface_definition() %}
private func uniffiTraitInterfaceCallAsync<T: Sendable>(
    makeCall: @escaping @Sendable () async throws -> T,
    handleSuccess: @escaping @Sendable (T) -> (),
    handleError: @escaping @Sendable (Int8, RustBuffer) -> (),
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>,
    lowerError: (@Sendable (Swift.Error) -> RustBuffer?)? = nil,
    discardResult: (@Sendable (T) async -> Void)? = nil
) {
    let task = Task {
        // Never race a cancellation acknowledgement against makeCall. Its async
        // frame (including defer) must return before the one FFI completion.
        var outcome: Result<T, Swift.Error>?
        do {
            outcome = .success(try await makeCall())
        } catch {
            outcome = .failure(error)
        }
        if Task.isCancelled {
            if case let .success(value) = outcome {
                await discardResult?(value)
            }
            // Release an unclaimed host object/stream before acknowledging cancel;
            // do not lower it into a new foreign handle that nobody can consume.
            outcome = nil
            handleError(CALL_CANCELLED, RustBuffer.empty())
            return
        }
        switch outcome! {
        case let .success(value):
            handleSuccess(value)
        case .failure(is CancellationError):
            handleError(CALL_CANCELLED, RustBuffer.empty())
        case let .failure(error):
            if let buffer = lowerError?(error) {
                handleError(CALL_ERROR, buffer)
            } else {
                handleError(CALL_UNEXPECTED_ERROR, {{ Type::String.borrow()|lower_fn }}(String(describing: error)))
            }
        }
    }
    let handle = UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.insert(obj: task)
    droppedCallback.pointee = UniffiForeignFutureDroppedCallbackStruct(
        handle: handle,
        free: uniffiForeignFutureDroppedCallback
    )
}

private func uniffiTraitInterfaceCallAsyncWithError<T: Sendable, E>(
    makeCall: @escaping @Sendable () async throws -> T,
    handleSuccess: @escaping @Sendable (T) -> (),
    handleError: @escaping @Sendable (Int8, RustBuffer) -> (),
    lowerError: @escaping @Sendable (E) -> RustBuffer,
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>,
    discardResult: (@Sendable (T) async -> Void)? = nil
) {
    uniffiTraitInterfaceCallAsync(
        makeCall: makeCall,
        handleSuccess: handleSuccess,
        handleError: handleError,
        droppedCallback: droppedCallback,
        lowerError: { error in
            guard let error = error as? E else { return nil }
            return lowerError(error)
        },
        discardResult: discardResult
    )
}

// Borrow the callback handle map implementation to store foreign future handles
// TODO: consolidate the handle-map code (https://github.com/mozilla/uniffi-rs/pull/1823)
fileprivate let UNIFFI_FOREIGN_FUTURE_HANDLE_MAP = UniffiHandleMap<UniffiForeignFutureTask>()

// Protocol for tasks that handle foreign futures.
//
// Defining a protocol allows all tasks to be stored in the same handle map.  This can't be done
// with the task object itself, since has generic parameters.
fileprivate protocol UniffiForeignFutureTask {
    func cancel()
}

extension Task: UniffiForeignFutureTask {}

private func uniffiForeignFutureDroppedCallback(handle: UInt64) {
    do {
        let task = try UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.remove(handle: handle)
        // Set the cancellation flag on the task.  If it's still running, the code can check the
        // cancellation flag or call `Task.checkCancellation()`.  If the task has completed, this is
        // a no-op.
        task.cancel()
    } catch {
        print("uniffiForeignFutureDroppedCallback: handle missing from handlemap")
    }
}

// For testing
public func uniffiForeignFutureHandleCount{{ ci.namespace()|class_name }}() -> Int {
    UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.count
}

{%- endif %}
