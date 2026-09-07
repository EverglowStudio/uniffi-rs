fileprivate protocol UniffiInputStreamProtocol: AnyObject {
    func next(
        callback: @escaping UniffiForeignFutureCompleteRustBuffer,
        callbackData: UInt64,
        droppedCallback: UnsafeMutablePointer<UniffiForeignFutureDroppedCallbackStruct>
    )
    func cancel()
    func close() async
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

fileprivate final class UniffiAsyncSequenceInputStream<S: AsyncSequence>: UniffiInputStreamProtocol, @unchecked Sendable {
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
    private var closeTask: Task<Void, Never>?
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
            taskBox.set(task: Task {
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

    func close() async {
        cancel()
        let task = lock.withLock {
            if let task = closeTask {
                return task
            }
            let last = lastTask
            let task = Task {
                await last?.wait()
                self.releaseIterator()
            }
            closeTask = task
            return task
        }
        await task.value
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
            let value = try await nextIterator.next()
            finishNext(iterator: value == nil ? nil : nextIterator)
            completeSuccess(value, callback: callback, callbackData: callbackData)
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
        if let errorBuf = lowerError(error) {
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
}

fileprivate func uniffiCreateInputStream<S: AsyncSequence>(
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
    Task {
        await stream.close()
        _ = try? UNIFFI_INPUT_STREAM_HANDLE_MAP.remove(handle: handle)
        // This also consumes the Rust completion sender when its waiter was dropped.
        callback(callbackData, UniffiForeignFutureResultVoid(callStatus: RustCallStatus()))
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
