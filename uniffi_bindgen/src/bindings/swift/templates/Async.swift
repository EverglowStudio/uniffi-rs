private let UNIFFI_RUST_FUTURE_POLL_READY: Int8 = 0
private let UNIFFI_RUST_FUTURE_POLL_WAKE: Int8 = 1

{%- if include_stream_runtime %}
// The Rust output-stream ABI completes one strict tagged StreamStep as a
// RustBuffer. `Item(nil)` is an item payload when Element is Optional; only a
// distinct Done tag completes the stream, and Error keeps the typed payload.
//
// This is internal so stream functions emitted by other component sources in the
// same Swift module can construct the shared UniFfiStream runtime.
enum __UniffiStreamNext<T> {
    case item(T)
    case done
    case error(Swift.Error)
}
{%- endif %}

{%- if ci.has_stream_fns() %}
private func __uniffiLiftStreamNext<T>(
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
    private let cancelFunc: (UInt64) -> ()
    private let freeFunc: (UInt64) -> ()
    private var cancelled = false
    private var freed = false

    init(
        handle: UInt64,
        cancelFunc: @escaping (UInt64) -> (),
        freeFunc: @escaping (UInt64) -> ()
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

fileprivate func uniffiRustCallAsync<F, T>(
    rustFutureFunc: () -> UInt64,
    pollFunc: (UInt64, @escaping UniffiRustFutureContinuationCallback, UInt64) -> (),
    cancelFunc: @escaping (UInt64) -> (),
    completeFunc: (UInt64, UnsafeMutablePointer<RustCallStatus>) -> F,
    freeFunc: @escaping (UInt64) -> (),
    liftFunc: (F) throws -> T,
    errorHandler: ((RustBuffer) throws -> Swift.Error)?
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
// Output streams use a pull-based custom AsyncSequence rather than a
// continuation-backed producer. Creating the public stream only captures the native
// start closure; it invokes neither Rust start nor `stream_next`. The first
// Iterator.next() starts Rust and creates one native `stream_next` future, so a slow
// Swift consumer cannot cause background prefetch.
public struct UniFfiStream<Element>: AsyncSequence {
    public struct Iterator: AsyncIteratorProtocol {
        fileprivate let lifetime: UniFfiStreamIteratorLifetime<Element>

        public mutating func next() async throws -> Element? {
            try await lifetime.next()
        }
    }

    fileprivate let state: UniFfiStreamState<Element>

    init(
        start: @escaping () throws -> UInt64,
        next: @escaping (UInt64) async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping (UInt64) -> ()
    ) {
        state = UniFfiStreamState(start: start, next: next, cancel: cancel)
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(lifetime: UniFfiStreamIteratorLifetime(state: state))
    }

    /// Cancel the stream. Cancelling before its first `next()` is local-only; an
    /// active native stream is cancelled at most once.
    public func cancel() {
        state.cancel()
    }
}

fileprivate final class UniFfiStreamIteratorLifetime<Element> {
    private let state: UniFfiStreamState<Element>
    private let accepted: Bool

    init(state: UniFfiStreamState<Element>) {
        self.state = state
        accepted = state.claimIterator()
    }

    deinit {
        if accepted {
            state.cancel()
        }
    }

    func next() async throws -> Element? {
        guard accepted else {
            throw UniffiInternalError.streamConsumed
        }
        return try await state.next()
    }
}

fileprivate final class UniFfiStreamState<Element>: @unchecked Sendable {
    private enum Lifecycle {
        case idle
        case starting
        case active(UInt64)
        case done
        case failed
        case cancelled
    }

    private enum NextStart {
        case next(UInt64)
        case done
        case concurrent
    }

    private let lock = NSLock()
    private let startFunc: () throws -> UInt64
    private let nextFunc: (UInt64) async throws -> __UniffiStreamNext<Element>
    private let cancelFunc: (UInt64) -> ()
    private var iteratorClaimed = false
    private var nextInFlight = false
    private var lifecycle = Lifecycle.idle

    init(
        start: @escaping () throws -> UInt64,
        next: @escaping (UInt64) async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping (UInt64) -> ()
    ) {
        startFunc = start
        nextFunc = next
        cancelFunc = cancel
    }

    deinit {
        cancel()
    }

    func claimIterator() -> Bool {
        lock.withLock {
            guard !iteratorClaimed else {
                return false
            }
            iteratorClaimed = true
            return true
        }
    }

    func next() async throws -> Element? {
        if Task.isCancelled {
            cancel()
            throw CancellationError()
        }

        let start = try beginNext()

        switch start {
        case .done:
            return nil
        case .concurrent:
            throw UniffiInternalError.concurrentStreamNext
        case let .next(handle):
            do {
                let result = try await withTaskCancellationHandler(operation: {
                    try await nextFunc(handle)
                }, onCancel: {
                    self.cancel()
                })
                if Task.isCancelled {
                    finishCancelledNext()
                    throw CancellationError()
                }
                return try finish(result)
            } catch is CancellationError {
                finishCancelledNext()
                throw CancellationError()
            } catch {
                finishFailedNext()
                throw error
            }
        }
    }

    private func beginNext() throws -> NextStart {
        try lock.withLock { () throws -> NextStart in
            switch lifecycle {
            case .done, .failed, .cancelled:
                return .done
            case .idle:
                if nextInFlight {
                    return .concurrent
                }
                nextInFlight = true
                lifecycle = .starting
                do {
                    // Keep start and the active handle transition under the same
                    // lock: cancellation observes either an idle stream or a fully
                    // initialized native handle, never a half-started stream.
                    let handle = try startFunc()
                    lifecycle = .active(handle)
                    return .next(handle)
                } catch {
                    nextInFlight = false
                    lifecycle = .failed
                    throw error
                }
            case .starting:
                return .concurrent
            case let .active(handle):
                if nextInFlight {
                    return .concurrent
                }
                nextInFlight = true
                return .next(handle)
            }
        }
    }

    private func finish(_ result: __UniffiStreamNext<Element>) throws -> Element? {
        return try lock.withLock { () throws -> Element? in
            nextInFlight = false
            switch lifecycle {
            case .cancelled, .done, .failed:
                return nil
            case .idle, .starting:
                throw UniffiInternalError.unexpectedStaleHandle
            case .active:
                switch result {
                case .done:
                    lifecycle = .done
                    return nil
                case let .error(error):
                    lifecycle = .failed
                    throw error
                case let .item(item):
                    // `Element?` adds an outer optional here. In particular, when
                    // Element is Optional<T>, this returns `.some(nil)` for Item(None).
                    return .some(item)
                }
            }
        }
    }

    private func finishFailedNext() {
        let handle = lock.withLock { () -> UInt64? in
            nextInFlight = false
            guard case let .active(activeHandle) = lifecycle else {
                return nil
            }
            lifecycle = .failed
            return activeHandle
        }
        if let handle {
            cancelFunc(handle)
        }
    }

    private func finishCancelledNext() {
        cancel()
    }

    func cancel() {
        let handle = lock.withLock { () -> UInt64? in
            nextInFlight = false
            switch lifecycle {
            case .idle, .starting:
                lifecycle = .cancelled
                return nil
            case let .active(activeHandle):
                lifecycle = .cancelled
                return activeHandle
            case .done, .failed, .cancelled:
                return nil
            }
        }
        if let handle {
            cancelFunc(handle)
        }
    }
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
private func uniffiTraitInterfaceCallAsync<T>(
    makeCall: @escaping @Sendable () async throws -> T,
    handleSuccess: @escaping @Sendable (T) -> (),
    handleError: @escaping @Sendable (Int8, RustBuffer) -> (),
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
) {
    let task = Task {
        // Note: it's important we call either `handleSuccess` or `handleError` exactly once.  Each
        // call consumes an Arc reference, which means there should be no possibility of a double
        // call.  The following code is structured so that will will never call both `handleSuccess`
        // and `handleError`, even in the face of weird errors.
        //
        // On platforms that need extra machinery to make C-ABI calls, like JNA or ctypes, it's
        // possible that we fail to make either call.  However, it doesn't seem like this is
        // possible on Swift since swift can just make the C call directly.
        var callResult: T
        do {
            callResult = try await makeCall()
        } catch {
            handleError(CALL_UNEXPECTED_ERROR, {{ Type::String.borrow()|lower_fn }}(String(describing: error)))
            return
        }
        handleSuccess(callResult)
    }
    let handle = UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.insert(obj: task)
    droppedCallback.pointee = UniffiForeignFutureDroppedCallbackStruct(
        handle: handle,
        free: uniffiForeignFutureDroppedCallback
    )
}

private func uniffiTraitInterfaceCallAsyncWithError<T, E>(
    makeCall: @escaping @Sendable () async throws -> T,
    handleSuccess: @escaping @Sendable (T) -> (),
    handleError: @escaping @Sendable (Int8, RustBuffer) -> (),
    lowerError: @escaping @Sendable (E) -> RustBuffer,
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
) {
    let task = Task {
        // See the note in uniffiTraitInterfaceCallAsync for details on `handleSuccess` and
        // `handleError`.
        var callResult: T
        do {
            callResult = try await makeCall()
        } catch let error as E {
            handleError(CALL_ERROR, lowerError(error))
            return
        } catch {
            handleError(CALL_UNEXPECTED_ERROR, {{ Type::String.borrow()|lower_fn }}(String(describing: error)))
            return
        }
        handleSuccess(callResult)
    }
    let handle = UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.insert(obj: task)
    droppedCallback.pointee = UniffiForeignFutureDroppedCallbackStruct(
        handle: handle,
        free: uniffiForeignFutureDroppedCallback
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
