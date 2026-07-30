private let UNIFFI_RUST_FUTURE_POLL_READY: Int8 = 0
private let UNIFFI_RUST_FUTURE_POLL_WAKE: Int8 = 1

// The Rust output-stream ABI completes `Result<Option<Item>, Error>` as a
// RustBuffer. Decode its outer Option tag before lifting Item: when Item is
// Optional, an inner `.none` is a valid item rather than stream completion.
private enum __UniffiStreamNext<T> {
    case item(T)
    case done
}

private func __uniffiLiftStreamNext<T>(
    _ rustBuffer: RustBuffer,
    readItem: (inout (data: Data, offset: Data.Index)) throws -> T
) throws -> __UniffiStreamNext<T> {
    defer {
        rustBuffer.deallocate()
    }
    guard rustBuffer.len > 0, rustBuffer.data != nil else {
        throw UniffiInternalError.bufferOverflow
    }
    var reader = createReader(data: Data(rustBuffer: rustBuffer))
    switch try readInt(&reader) as Int8 {
    case 0:
        guard !hasRemaining(reader) else {
            throw UniffiInternalError.incompleteData
        }
        return .done
    case 1:
        let item = try readItem(&reader)
        guard !hasRemaining(reader) else {
            throw UniffiInternalError.incompleteData
        }
        return .item(item)
    default:
        throw UniffiInternalError.unexpectedOptionalTag
    }
}

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

// Output streams use a pull-based custom AsyncSequence instead of an
// AsyncThrowingStream continuation. A native `stream_next` future is created only
// from Iterator.next(), so a slow Swift consumer cannot cause background prefetch.
public struct UniffiAsyncStream<Element>: AsyncSequence {
    public struct Iterator: AsyncIteratorProtocol {
        fileprivate let lifetime: UniffiAsyncStreamIteratorLifetime<Element>

        public mutating func next() async throws -> Element? {
            try await lifetime.next()
        }
    }

    fileprivate let state: UniffiAsyncStreamState<Element>

    fileprivate init(
        next: @escaping () async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping () -> ()
    ) {
        state = UniffiAsyncStreamState(next: next, cancel: cancel)
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(lifetime: UniffiAsyncStreamIteratorLifetime(state: state))
    }
}

fileprivate final class UniffiAsyncStreamIteratorLifetime<Element> {
    private let state: UniffiAsyncStreamState<Element>
    private let accepted: Bool

    init(state: UniffiAsyncStreamState<Element>) {
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

fileprivate final class UniffiAsyncStreamState<Element>: @unchecked Sendable {
    private enum NextStart {
        case next
        case done
        case concurrent
    }

    private let lock = NSLock()
    private let nextFunc: () async throws -> __UniffiStreamNext<Element>
    private let cancelFunc: () -> ()
    private var iteratorClaimed = false
    private var nextInFlight = false
    private var terminal = false
    private var cancelled = false

    init(
        next: @escaping () async throws -> __UniffiStreamNext<Element>,
        cancel: @escaping () -> ()
    ) {
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
        let start = lock.withLock { () -> NextStart in
            if terminal || cancelled {
                return .done
            }
            if nextInFlight {
                return .concurrent
            }
            nextInFlight = true
            return .next
        }

        switch start {
        case .done:
            return nil
        case .concurrent:
            throw UniffiInternalError.concurrentStreamNext
        case .next:
            break
        }

        if Task.isCancelled {
            finishCancelledNext()
            throw CancellationError()
        }

        do {
            let result = try await withTaskCancellationHandler(operation: {
                try await nextFunc()
            }, onCancel: {
                self.cancel()
            })
            if Task.isCancelled {
                finishCancelledNext()
                throw CancellationError()
            }
            return finish(result)
        } catch {
            finishFailedNext()
            throw error
        }
    }

    private func finish(_ result: __UniffiStreamNext<Element>) -> Element? {
        lock.withLock {
            nextInFlight = false
            if cancelled {
                return nil
            }
            switch result {
            case .done:
                terminal = true
                return nil
            case let .item(item):
                // `Element?` adds an outer optional here. In particular, when
                // Element is Optional<T>, this returns `.some(nil)` for Item(None).
                return .some(item)
            }
        }
    }

    private func finishFailedNext() {
        let cancel = lock.withLock { () -> (() -> ())? in
            nextInFlight = false
            guard !terminal && !cancelled else {
                return nil
            }
            terminal = true
            cancelled = true
            return cancelFunc
        }
        cancel?()
    }

    private func finishCancelledNext() {
        let cancel = lock.withLock { () -> (() -> ())? in
            nextInFlight = false
            guard !terminal && !cancelled else {
                return nil
            }
            terminal = true
            cancelled = true
            return cancelFunc
        }
        cancel?()
    }

    func cancel() {
        let cancel = lock.withLock { () -> (() -> ())? in
            guard !terminal && !cancelled else {
                return nil
            }
            terminal = true
            cancelled = true
            return cancelFunc
        }
        cancel?()
    }
}

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
