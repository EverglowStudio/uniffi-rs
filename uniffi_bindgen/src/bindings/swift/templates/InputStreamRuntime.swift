fileprivate protocol UniffiInputStreamProtocol: AnyObject, Sendable {
    func next(
        callback: @escaping UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64,
        droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
    )
    func cancel()
    func close() async throws
}

fileprivate final class UniffiInputStreamNextTaskBox: @unchecked Sendable {
    private let lock = NSLock()
    private var task: Task<Void, Never>?
    private var cancelled = false
    private var taskWaiters: [CheckedContinuation<Task<Void, Never>, Never>] = []

    func set(task: Task<Void, Never>) {
        let (cancelled, waiters) = lock.withLock {
            self.task = task
            let waiters = taskWaiters
            taskWaiters.removeAll()
            return (self.cancelled, waiters)
        }
        if cancelled {
            task.cancel()
        }
        for waiter in waiters {
            waiter.resume(returning: task)
        }
    }

    func cancel() {
        let task = lock.withLock {
            cancelled = true
            return self.task
        }
        task?.cancel()
    }

    func wait() async {
        // Accepted next calls usually observe an already-published predecessor.
        // Joining that Task needs no continuation for the publication itself.
        if let task = lock.withLock({ self.task }) {
            await task.value
            return
        }
        let task: Task<Void, Never> = await withCheckedContinuation { continuation in
            let task = lock.withLock { () -> Task<Void, Never>? in
                if let task = self.task {
                    return task
                }
                taskWaiters.append(continuation)
                return nil
            }
            if let task = task {
                continuation.resume(returning: task)
            }
        }
        await task.value
    }
}

fileprivate let UNIFFI_INPUT_STREAM_HANDLE_MAP = UniffiHandleMap<UniffiInputStreamProtocol>()
fileprivate let UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP = UniffiHandleMap<UniffiInputStreamNextTaskBox>()

fileprivate final class UniffiAsyncSequenceInputStream<S: AsyncSequence & Sendable>: UniffiInputStreamProtocol, @unchecked Sendable {
    private enum NextStart {
        case start
        case done
        case concurrent
    }

    private let lock = NSLock()
    private var iterator: S.AsyncIterator?
    private var pendingTask: UniffiInputStreamNextTaskBox?
    // Each accepted next joins its predecessor, including its completion callback.
    private var lastTask: UniffiInputStreamNextTaskBox?
    private var closeTask: Task<Void, Error>?
    private let closeSequence: (@Sendable () async throws -> Void)?
    private var cancelled = false
    private let lowerNext: (S.Element?) -> RustBuffer
    private let lowerError: (Swift.Error) -> RustBuffer?

    init(
        sequence: S,
        lowerNext: @escaping (S.Element?) -> RustBuffer,
        lowerError: @escaping (Swift.Error) -> RustBuffer?
    ) {
        self.iterator = sequence.makeAsyncIterator()
        self.lowerNext = lowerNext
        self.lowerError = lowerError
        if let sequence = sequence as? any UniffiAsyncSequenceClose {
            closeSequence = { try await sequence.close() }
        } else {
            closeSequence = nil
        }
    }

    func next(
        callback: @escaping UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64,
        droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
    ) {
        let taskBox = UniffiInputStreamNextTaskBox()
        var previousTask: UniffiInputStreamNextTaskBox?
        let start = lock.withLock {
            if cancelled {
                return NextStart.done
            }
            if pendingTask != nil {
                return NextStart.concurrent
            }
            if iterator == nil {
                return NextStart.done
            }
            previousTask = lastTask
            lastTask = taskBox
            pendingTask = taskBox
            return NextStart.start
        }

        switch start {
        case .done:
            completeSuccess(nil, callback: callback, callbackData: callbackData)
        case .concurrent:
            completeUnexpected(
                "UniFFI input stream received a concurrent next() request",
                callback: callback,
                callbackData: callbackData
            )
        case .start:
            let taskHandle = UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP.insert(obj: taskBox)
            droppedCallback.pointee = UniffiForeignFutureDroppedCallbackStruct(
                handle: taskHandle,
                free: uniffiInputStreamNextDroppedCallback
            )
            let predecessor = previousTask
            // C function pointers can be captured by a Sendable Task closure;
            // the accepted Task owns this callback/data pair's one completion.
            taskBox.set(task: Task { @Sendable in
                await self.runNext(
                    callback: callback,
                    callbackData: callbackData,
                    taskHandle: taskHandle
                )
                await predecessor?.wait()
            })
        }
    }

    func cancel() {
        let task = lock.withLock {
            cancelled = true
            return pendingTask
        }
        task?.cancel()
    }

    func close() async throws {
        cancel()
        let task = lock.withLock {
            if let task = closeTask {
                return task
            }
            let last = lastTask
            let task = Task {
                await last?.wait()
                self.releaseIterator()
                try await self.closeSequence?()
            }
            closeTask = task
            return task
        }
        try await task.value
    }

    private func releaseIterator() {
        // runNext has returned, including its local iterator and completion callback.
        // Release the idle iterator outside the lock (deinit may run user code).
        let iterator = lock.withLock {
            let iterator = self.iterator
            self.iterator = nil
            lastTask = nil
            return iterator
        }
        withExtendedLifetime(iterator) {}
    }

    private func runNext(
        callback: @escaping UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64,
        taskHandle: UInt64
    ) async {
        var nextIterator: S.AsyncIterator?
        lock.withLock {
            nextIterator = iterator
            iterator = nil
        }

        guard var nextIterator = nextIterator else {
            finishNext(iterator: nil)
            completeSuccess(nil, callback: callback, callbackData: callbackData)
            _ = try? UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP.remove(handle: taskHandle)
            return
        }

        do {
            var value = try await nextIterator.next()
            if Task.isCancelled {
                // No raw handles may be lowered into a result whose Rust waiter
                // has cancelled. Close still joins this Task and its iterator.
                value = nil
                finishNext(iterator: nil)
                completeCancelled(callback: callback, callbackData: callbackData)
            } else {
                finishNext(iterator: value == nil ? nil : nextIterator)
                completeSuccess(value, callback: callback, callbackData: callbackData)
            }
        } catch {
            finishNext(iterator: nil)
            completeError(error, callback: callback, callbackData: callbackData)
        }
        _ = try? UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP.remove(handle: taskHandle)
    }

    private func finishNext(iterator: S.AsyncIterator?) {
        lock.withLock {
            pendingTask = nil
            if !cancelled {
                self.iterator = iterator
            }
        }
    }

    private func completeSuccess(
        _ value: S.Element?,
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64
    ) {
        callback(
            callbackData,
            UniffiForeignFutureResultRustBuffer(
                returnValue: lowerNext(value),
                callStatus: RustCallStatus()
            )
        )
    }

    private func completeError(
        _ error: Swift.Error,
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64
    ) {
        if Task.isCancelled || error is CancellationError {
            completeCancelled(callback: callback, callbackData: callbackData)
        } else if let errorBuf = lowerError(error) {
            callback(
                callbackData,
                UniffiForeignFutureResultRustBuffer(
                    returnValue: RustBuffer.empty(),
                    callStatus: RustCallStatus(code: CALL_ERROR, errorBuf: errorBuf)
                )
            )
        } else {
            completeUnexpected(
                String(describing: error),
                callback: callback,
                callbackData: callbackData
            )
        }
    }

    private func completeCancelled(
        callback: UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64
    ) {
        callback(callbackData, UniffiForeignFutureResultRustBuffer(
            returnValue: RustBuffer.empty(),
            callStatus: RustCallStatus(code: CALL_CANCELLED, errorBuf: RustBuffer.empty())
        ))
    }
}

fileprivate func uniffiCreateInputStream<S: AsyncSequence & Sendable>(
    _ sequence: S,
    lowerNext: @escaping (S.Element?) -> RustBuffer,
    lowerError: @escaping (Swift.Error) -> RustBuffer?
) -> UInt64 {
    UNIFFI_INPUT_STREAM_HANDLE_MAP.insert(
        obj: UniffiAsyncSequenceInputStream(
            sequence: sequence,
            lowerNext: lowerNext,
            lowerError: lowerError
        )
    )
}

fileprivate func uniffiInputStreamNextCallback(
    handle: UInt64,
    callback: @escaping UniffiForeignFutureCompleteRustBuffer,
    callbackData: UInt64,
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
) {
    do {
        try UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle).next(
            callback: callback,
            callbackData: callbackData,
            droppedCallback: droppedCallback
        )
    } catch {
        completeUnexpected(
            "UniFFI input stream handle is no longer registered",
            callback: callback,
            callbackData: callbackData
        )
    }
}

fileprivate func uniffiInputStreamCancelCallback(handle: UInt64) {
    if let stream = try? UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle) {
        stream.cancel()
    }
}

fileprivate func uniffiInputStreamCloseCallback(
    handle: UInt64,
    callback: @escaping UniffiForeignFutureCompleteVoid,
    callbackData: UInt64,
    droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
) {
    // Dropping the Rust waiter requests cancellation but cannot cancel cleanup.
    droppedCallback.pointee = UniffiForeignFutureDroppedCallbackStruct(
        handle: handle,
        free: uniffiInputStreamCancelCallback
    )
    guard let stream = try? UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle) else {
        callback(callbackData, UniffiForeignFutureResultVoid(callStatus: RustCallStatus(
            code: CALL_UNEXPECTED_ERROR,
            errorBuf: {{ Type::String.borrow()|lower_fn }}("UniFFI input stream handle is no longer registered")
        )))
        return
    }
    // The Sendable Task closure captures the C function pointer and completes
    // exactly once after the joined cleanup, independently of waiter lifetime.
    Task { @Sendable in
        do {
            try await stream.close()
            _ = try? UNIFFI_INPUT_STREAM_HANDLE_MAP.remove(handle: handle)
            // Consume the sender even when the Rust waiter has been dropped.
            callback(callbackData, UniffiForeignFutureResultVoid(callStatus: RustCallStatus()))
        } catch {
            _ = try? UNIFFI_INPUT_STREAM_HANDLE_MAP.remove(handle: handle)
            callback(callbackData, UniffiForeignFutureResultVoid(callStatus: RustCallStatus(
                code: CALL_UNEXPECTED_ERROR,
                errorBuf: {{ Type::String.borrow()|lower_fn }}(String(describing: error))
            )))
        }
    }
}

private func uniffiInputStreamNextDroppedCallback(handle: UInt64) {
    if let task = try? UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP.remove(handle: handle) {
        task.cancel()
    }
}

private func completeUnexpected(
    _ message: String,
    callback: UniffiForeignFutureCompleteRustBuffer,
    callbackData: UInt64
) {
    callback(
        callbackData,
        UniffiForeignFutureResultRustBuffer(
            returnValue: RustBuffer.empty(),
            callStatus: RustCallStatus(
                code: CALL_UNEXPECTED_ERROR,
                errorBuf: {{ Type::String.borrow()|lower_fn }}(message)
            )
        )
    )
}

// For testing
public func uniffiInputStreamHandleCount{{ ci.namespace()|class_name }}() -> Int {
    UNIFFI_INPUT_STREAM_HANDLE_MAP.count
}

// The RustBuffer has one owner, transferred from the C completion to its waiter.
fileprivate struct UniffiInputStreamCompletion: @unchecked Sendable {
    let result: UniffiForeignFutureResultRustBuffer
}
fileprivate let UNIFFI_INPUT_STREAM_COMPLETIONS = UniffiHandleMap<CheckedContinuation<UniffiInputStreamCompletion, Never>>()

fileprivate func uniffiLiftInputStream<T: Sendable>(
    _ handle: UInt64,
    liftNext: @escaping @Sendable (RustBuffer) throws -> T?,
    errorHandler: (@Sendable (RustBuffer) throws -> Swift.Error)?
) throws -> UniFfiStream<T> {
    let stream = try UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle)
    return UniFfiStream(
        handle: handle,
        next: { _ in
            let completion = await withTaskCancellationHandler {
                await withCheckedContinuation { continuation in
                    let data = UNIFFI_INPUT_STREAM_COMPLETIONS.insert(obj: continuation)
                    var dropped = UniffiForeignFutureDroppedCallbackStruct(handle: 0, free: { _ in })
                    stream.next(callback: { data, result in
                        let continuation = try! UNIFFI_INPUT_STREAM_COMPLETIONS.remove(handle: data)
                        continuation.resume(returning: UniffiInputStreamCompletion(result: result))
                    }, callbackData: data, droppedCallback: &dropped)
                }
            } onCancel: {
                stream.cancel()
            }
            let value = try liftNext(makeRustCall({ status in
                status.pointee = completion.result.callStatus
                return completion.result.returnValue
            }, errorHandler: errorHandler))
            if let value { return .item(value) }
            return .done
        },
        cancel: { _ in stream.cancel() },
        close: { _ in try await stream.close() }
    )
}
