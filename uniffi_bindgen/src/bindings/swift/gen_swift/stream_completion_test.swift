
@main struct NativeCompletionGate {
    static func main() async throws {
        do {
            let future = FutureProbe()
            let cancelled = Signal(), freeEntered = Signal(), freed = Signal()
            let frameReturned = Signal(), closeEntered = Signal(), closeRelease = Signal(), closed = Signal()
            let freeRelease = DispatchSemaphore(value: 0)
            let stream = UniFfiStream<Int>(rustSubscriptionHandle: 7, next: { _ in
                defer { frameReturned.fire() }
                let value = try await uniffiRustCallAsync(
                    rustFutureFunc: { 7 },
                    pollFunc: { _, callback, data in future.poll(callback, data) },
                    cancelFunc: { _ in future.cancel() },
                    completeFunc: { _, _ in 7 },
                    freeFunc: { _ in
                        freeEntered.fire()
                        freeRelease.wait()
                        future.free()
                        freed.fire()
                    }, liftFunc: { $0 }, errorHandler: nil)
                return .item(value)
            }, cancel: { _ in cancelled.fire(); future.signal(0) }, close: { _ in
                precondition(freed.isSet && frameReturned.isSet, "close preceded raw-next frame exit")
                closeEntered.fire()
                await closeRelease.wait()
                closed.fire()
            })
            let initialIterator = stream.makeAsyncIterator()
            let worker = Task { var iterator = initialIterator; return try await iterator.next() }
            await future.entered.wait()
            var concurrent = initialIterator
            do { _ = try await concurrent.next(); preconditionFailure("accepted concurrent native next") }
            catch UniffiInternalError.concurrentStreamNext {}
            stream.cancel()
            await cancelled.wait()
            await freeEntered.wait()
            let close1 = Task { try await stream.close() }
            let close2 = Task { try await stream.close() }
            close2.cancel()
            precondition(!closeEntered.isSet && !closed.isSet && future.freeCount == 0)
            freeRelease.signal()
            await closeEntered.wait()
            precondition(!closed.isSet)
            closeRelease.fire()
            try await close1.value
            try await close2.value
            do { _ = try await worker.value; preconditionFailure("cancel delivered an item") }
            catch is CancellationError {}
            precondition(!worker.isCancelled, "subscription cancellation cancelled the caller Task")
            precondition(future.freeCount == 1 && future.cancelCount == 0)
            print("PASS native subscription cancel waits complete/free/frame and shared close; caller remains uncancelled")
        }
        do {
            let entered = Signal(), cancelled = Signal(), start = Signal(), returned = Signal(), closed = Signal()
            let stream = UniFfiStream<Int>(rustSubscriptionHandle: 8, next: { _ in
                defer { returned.fire() }
                entered.fire()
                await start.wait()
                precondition(cancelled.isSet)
                return .done
            }, cancel: { _ in cancelled.fire() }, close: { _ in
                precondition(returned.isSet)
                closed.fire()
            })
            let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
            await entered.wait()
            let closer = Task { try await stream.close() }
            await cancelled.wait()
            precondition(!closed.isSet)
            start.fire()
            try await closer.value
            do { _ = try await worker.value; preconditionFailure("cancel before driver entry became EOF") }
            catch is CancellationError {}
            print("PASS reserved native next waits for its actual return after early subscription close")
        }
        for fails in [false, true] {
            let returned = Signal(), closed = Signal()
            let stream = UniFfiStream<Int>(handle: 9, next: { _ in
                defer { returned.fire() }
                if fails { throw UniffiInternalError.unexpectedStaleHandle }
                return .done
            }, cancel: { _ in }, close: { _ in
                precondition(returned.isSet)
                closed.fire()
            })
            var iterator = stream.makeAsyncIterator()
            do { let value = try await iterator.next(); precondition(!fails && value == nil) }
            catch UniffiInternalError.unexpectedStaleHandle { precondition(fails) }
            precondition(closed.isSet)
            try await stream.close()
            print("PASS generic terminal next confirms before its own close: error=\(fails)")
        }
        do {
            let entered = Signal(), cancelled = Signal(), released = Signal(), closed = Signal()
            let stream = UniFfiStream<HostValue>(rustSubscriptionHandle: 11, next: { _ in
                entered.fire()
                await cancelled.wait()
                return .item(HostValue(released))
            }, cancel: { _ in cancelled.fire() }, close: { _ in
                precondition(released.isSet, "close preceded unclaimed result release")
                closed.fire()
            })
            let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
            await entered.wait()
            try await stream.close()
            do { _ = try await worker.value; preconditionFailure("cancel delivered an owned value") }
            catch is CancellationError {}
            precondition(closed.isSet)
            print("PASS native cancellation releases an unclaimed result before close ACK")
        }
        do {
            let steps = StreamSteps<Int?>([.item(nil), .item(3), .done])
            let closed = Signal()
            let stream = UniFfiStream<Int?>(handle: 10,
                next: { _ in steps.next() }, cancel: { _ in }, close: { _ in closed.fire() })
            var iterator = stream.makeAsyncIterator()
            precondition(steps.count == 0)
            let first = try await iterator.next()
            precondition(first != nil && first! == nil)
            let second = try await iterator.next()
            precondition(second! == 3)
            let done = try await iterator.next()
            precondition(done == nil && closed.isSet && steps.count == 3)
            print("PASS generic optional item, no prefetch and EOF")
        }
        precondition(uniffiContinuationHandleMap.count == 0)
        try await mergedRustFrameCases()
        try await terminalAcknowledgementCases()
        try await mergedItemCases()
        try await consumingTerminalCases()
        print("6 original completion scenarios and opt3 terminal acknowledgement cases passed")
    }
}

fileprivate final class Opt7OwnedValue: Sendable {
    let dispose: @Sendable () -> Void
    init(_ dispose: @escaping @Sendable () -> Void) { self.dispose = dispose }
    deinit { dispose() }
}

fileprivate final class Opt7ReleaseProbe: @unchecked Sendable {
    private let lock = NSLock()
    private weak var state: UniFfiStreamState<Opt7OwnedValue>?
    let reentered = Signal(), finished = Signal()
    let release = DispatchSemaphore(value: 0)
    func install(_ state: UniFfiStreamState<Opt7OwnedValue>) {
        lock.withLock { self.state = state }
    }
    func dispose() {
        // A synchronous close request must not deadlock in the value's deinit.
        let state = lock.withLock { self.state }
        state?.cancel()
        reentered.fire()
        release.wait()
        finished.fire()
    }
}

fileprivate func mergedItemCases() async throws {
    do {
        let steps = StreamSteps<Int?>([.item(nil), .item(3)])
        let future = FutureProbe(), returned = Signal(), closed = Signal()
        let stream = UniFfiStream<Int?>(rustSubscriptionHandle: 50, next: { _ in
            defer { returned.fire() }
            return try await uniffiRustCallAsync(
                rustFutureFunc: { 50 }, pollFunc: { _, callback, data in callback(data, 0) },
                cancelFunc: { _ in }, completeFunc: { _, _ in 0 },
                freeFunc: { _ in future.free() }, liftFunc: { _ in steps.next() }, errorHandler: nil)
        }, cancel: { _ in }, close: { _ in
            precondition(future.freeCount == 2 && returned.isSet && !closed.isSet)
            closed.fire()
        })
        var iterator = stream.makeAsyncIterator()
        let first = try await iterator.next()
        precondition(first != nil && first! == nil && future.freeCount == 1 && returned.isSet)
        let second = try await iterator.next()
        precondition(second! == 3 && future.freeCount == 2 && !closed.isSet)
        try await stream.close(); try await stream.close()
        precondition(closed.isSet && steps.count == 2)
        print("PASS opt7 native optional items follow raw free/frame and retain real close")
    }
    do {
        let delivered = Signal(), release = Signal(), closed = Signal()
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 51,
            next: { _ in .item(7) }, cancel: { _ in }, close: { _ in closed.fire() })
        let worker = Task {
            var iterator = stream.makeAsyncIterator()
            let value = try await iterator.next()
            delivered.fire()
            await release.wait()
            return value
        }
        await delivered.wait()
        worker.cancel()
        try await stream.close()
        release.fire()
        let value = try await worker.value
        precondition(value == 7 && closed.isSet)
        print("PASS opt7 cancellation after item delivery retains the delivered value")
    }
    for closeFails in [false, true] {
        let entered = Signal(), nextRelease = Signal(), cancelled = Signal()
        let freed = Signal(), returned = Signal(), closeEntered = Signal(), closeRelease = Signal()
        let probe = Opt7ReleaseProbe()
        let stream = UniFfiStream<Opt7OwnedValue>(rustSubscriptionHandle: 52, next: { _ in
            defer { returned.fire() }
            entered.fire(); await nextRelease.wait()
            return try await uniffiRustCallAsync(
                rustFutureFunc: { 52 }, pollFunc: { _, callback, data in callback(data, 0) },
                cancelFunc: { _ in }, completeFunc: { _, _ in 0 }, freeFunc: { _ in freed.fire() },
                liftFunc: { _ in .item(Opt7OwnedValue { probe.dispose() }) }, errorHandler: nil)
        }, cancel: { _ in cancelled.fire() }, close: { _ in
            precondition(freed.isSet && returned.isSet && probe.finished.isSet)
            precondition(!closeEntered.isSet)
            closeEntered.fire(); await closeRelease.wait()
            if closeFails { throw TerminalGateError.close }
        })
        probe.install(stream.state)
        let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
        await entered.wait()
        let owner = Task { try await stream.close() }
        await cancelled.wait()
        let waiter = Task { try await stream.close() }
        waiter.cancel()
        nextRelease.fire()
        await probe.reentered.wait()
        precondition(freed.isSet && returned.isSet && !probe.finished.isSet && !closeEntered.isSet)
        probe.release.signal()
        await closeEntered.wait()
        closeRelease.fire()
        for task in [owner, waiter] {
            do { try await task.value; precondition(!closeFails) }
            catch TerminalGateError.close { precondition(closeFails) }
        }
        do { _ = try await worker.value; preconditionFailure("unclaimed item delivered") }
        catch TerminalGateError.close { precondition(closeFails) }
        catch is CancellationError { precondition(!closeFails) }
        do { try await stream.close(); precondition(!closeFails) }
        catch TerminalGateError.close { precondition(closeFails) }
        print("PASS opt7 close winner waits lock-free reentrant value deinit and preserves owner: failure=\(closeFails)")
    }
    do {
        let entered = Signal(), nextRelease = Signal(), cancelled = Signal(), disposed = Signal(), closed = Signal()
        let stream = UniFfiStream<Opt7OwnedValue>(rustSubscriptionHandle: 53, next: { _ in
            entered.fire(); await nextRelease.wait()
            return .item(Opt7OwnedValue { disposed.fire() })
        }, cancel: { _ in cancelled.fire() }, close: { _ in
            precondition(disposed.isSet); closed.fire()
        })
        let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
        await entered.wait()
        worker.cancel()
        await cancelled.wait()
        precondition(!closed.isSet)
        nextRelease.fire()
        do { _ = try await worker.value; preconditionFailure("cancelled Task received an item") }
        catch is CancellationError {}
        precondition(disposed.isSet && closed.isSet)
        print("PASS opt7 Task cancelled before item decision disposes before close")
    }
    do {
        let entered = Signal(), handlerEntered = Signal(), closed = Signal()
        let release = DispatchSemaphore(value: 0)
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 54,
            next: { _ in .done }, cancel: { _ in }, close: { _ in closed.fire() })
        // Use the actual production state lock and real Task cancellation. The
        // cancelling thread's handler waits for this lock while its owner reads
        // Task.isCancelled. A getter that waited for the handler would deadlock.
        let stateLock = Mirror(reflecting: stream.state).children.first { $0.label == "lock" }!.value as! NSLock
        let worker = Task {
            await withTaskCancellationHandler {
                stateLock.withLock {
                    entered.fire()
                    release.wait()
                    return Task.isCancelled
                }
            } onCancel: { handlerEntered.fire(); stream.cancel() }
        }
        await entered.wait()
        let canceller = Task.detached { worker.cancel() }
        await handlerEntered.wait()
        precondition(worker.isCancelled)
        release.signal()
        let observed = await worker.value
        await canceller.value
        try await stream.close()
        precondition(observed && closed.isSet)
        print("PASS opt7 Swift getter reads cancellation under state lock while handler waits")
    }
    print("6 opt7 item/close/cancellation scenarios passed")
}

fileprivate enum TerminalGateError: Error { case business, close, lift }

fileprivate func terminalAcknowledgementCases() async throws {
    for business in [false, true] {
        let freeEntered = Signal(), freed = Signal(), returned = Signal(), closed = Signal()
        let freeRelease = DispatchSemaphore(value: 0)
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 20, next: { _ in
            defer { returned.fire() }
            return try await uniffiRustCallAsync(
                rustFutureFunc: { 20 }, pollFunc: { _, callback, data in callback(data, 0) },
                cancelFunc: { _ in }, completeFunc: { _, _ in 0 },
                freeFunc: { _ in freeEntered.fire(); freeRelease.wait(); freed.fire() },
                liftFunc: { _ -> __UniffiStreamNext<Int> in
                    business ? .error(TerminalGateError.business) : .done
                }, errorHandler: nil)
        }, cancel: { _ in }, close: { _ in closed.fire() })
        let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
        await freeEntered.wait()
        precondition(!freed.isSet && !returned.isSet && !closed.isSet)
        freeRelease.signal()
        do { let value = try await worker.value; precondition(!business && value == nil) }
        catch TerminalGateError.business { precondition(business) }
        precondition(freed.isSet && returned.isSet && !closed.isSet)
        try await stream.close(); try await stream.close()
        precondition(!closed.isSet)
        print("PASS native typed terminal reuses ACK after free/frame: business=\(business)")
    }
    for business in [false, true] {
        let entered = Signal(), cancel = Signal(), nextRelease = Signal(), returned = Signal()
        let closeEntered = Signal(), closeRelease = Signal(), ownerReturned = Signal(), waiterReturned = Signal()
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 21, next: { _ in
            defer { returned.fire() }
            entered.fire(); await nextRelease.wait()
            return business ? .error(TerminalGateError.business) : .done
        }, cancel: { _ in cancel.fire() }, close: { _ in
            precondition(returned.isSet)
            precondition(!closeEntered.isSet, "duplicate close owner")
            closeEntered.fire(); await closeRelease.wait(); throw TerminalGateError.close
        })
        let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
        await entered.wait()
        let owner = Task { defer { ownerReturned.fire() }; try await stream.close() }
        await cancel.wait()
        let waiter = Task { defer { waiterReturned.fire() }; try await stream.close() }
        waiter.cancel()
        precondition(!returned.isSet && !closeEntered.isSet)
        nextRelease.fire(); await closeEntered.wait()
        precondition(!ownerReturned.isSet && !waiterReturned.isSet)
        closeRelease.fire()
        do { try await owner.value; preconditionFailure("lost close failure") } catch TerminalGateError.close {}
        do { try await waiter.value; preconditionFailure("lost shared failure") } catch TerminalGateError.close {}
        do { _ = try await worker.value; preconditionFailure("terminal overwrote close failure") } catch TerminalGateError.close {}
        do { try await stream.close(); preconditionFailure("lost repeated failure") } catch TerminalGateError.close {}
        print("PASS native terminal preserves prior close owner/frame/failure: business=\(business)")
    }
    for cancelled in [false, true] {
        let freed = Signal(), returned = Signal(), closed = Signal()
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 22, next: { _ in
            defer { returned.fire() }
            return try await uniffiRustCallAsync(
                rustFutureFunc: { 22 }, pollFunc: { _, callback, data in callback(data, 0) },
                cancelFunc: { _ in }, completeFunc: { _, status in
                    if cancelled { status.pointee.code = 3 }
                    return 0
                }, freeFunc: { _ in freed.fire() },
                liftFunc: { _ -> __UniffiStreamNext<Int> in throw TerminalGateError.lift }, errorHandler: nil)
        }, cancel: { _ in }, close: { _ in
            precondition(freed.isSet && returned.isSet && !closed.isSet)
            closed.fire()
        })
        var iterator = stream.makeAsyncIterator()
        do { _ = try await iterator.next(); preconditionFailure("driver failure became terminal success") }
        catch is CancellationError { precondition(cancelled) }
        catch TerminalGateError.lift { precondition(!cancelled) }
        precondition(closed.isSet)
        try await stream.close()
        print("PASS native driver failure keeps real close: cancelled=\(cancelled)")
    }
    for pulled in [false, true] {
        let nextEntered = Signal(), closed = Signal()
        let stream = UniFfiStream<Int>(rustSubscriptionHandle: 23, next: { _ in
            nextEntered.fire(); return .item(1)
        }, cancel: { _ in }, close: { _ in
            precondition(!closed.isSet); closed.fire()
        })
        if pulled {
            var iterator = stream.makeAsyncIterator()
            let value = try await iterator.next(); precondition(value == 1)
            try await stream.close()
        } else { try await stream.close() }
        try await stream.close()
        precondition(closed.isSet && nextEntered.isSet == pulled)
        print("PASS native nonterminal/uncollected close preserved: pulled=\(pulled)")
    }
    print("8 opt3 native terminal ACK scenarios passed")
}

fileprivate func mergedRustFrameCases() async throws {
    do {
        let lock = NSLock()
        let sameLockWaiter = UniFfiRustNextCloseWaiter(lock: lock)
        let continuation = lock.withLock { sameLockWaiter.finishLocked() }
        precondition(continuation == nil)
        await sameLockWaiter.wait()
        print("PASS S1 finish before close waiter registration cannot lose confirmation")
    }
    do {
        let lock = NSLock()
        let waiter = UniFfiRustNextCloseWaiter(lock: lock), resumed = Signal()
        let worker = Task { await waiter.wait(); resumed.fire() }
        // Synchronize on the production lock to observe actual registration,
        // rather than assuming a Task has run after yield or sleep.
        while !lock.withLock({
            let value = Mirror(reflecting: waiter).children.first { $0.label == "continuation" }!.value
            return !Mirror(reflecting: value).children.isEmpty
        }) { await Task.yield() }
        precondition(!resumed.isSet)
        let continuation = lock.withLock { waiter.finishLocked() }
        precondition(continuation != nil && !resumed.isSet)
        continuation?.resume()
        await worker.value
        precondition(resumed.isSet)
        print("PASS S1 registered close waiter resumes only after locked frame finish")
    }
    do {
        let closed = Signal()
        weak var released: UniFfiStreamState<Int>?
        do {
            let state = UniFfiStreamState<Int>(handle: 42, next: { _ in .done }, cancel: { _ in }, close: { _ in closed.fire() }, execution: .rustSubscription)
            released = state
        }
        await closed.wait()
        precondition(released == nil)
        print("PASS S1 deinit close does not retain or resurrect its state")
    }
    print("3 S1 merged state protocol scenarios passed")
}

fileprivate enum Opt8LiftFailure: Error { case expected }

fileprivate func consumingTerminalCases() async throws {
    // The real lifecycle lock waits for an already entered cancel, then prevents
    // subsequent handle access. No terminal/Rust conversion runs inside that lock.
    do {
        let entered = Signal(), cancelled = Signal(), handed = Signal()
        let release = DispatchSemaphore(value: 0)
        let handle = UniffiRustFutureHandle(handle: 1, cancelFunc: { _ in
            entered.fire(); release.wait(); cancelled.fire()
        }, freeFunc: { _ in preconditionFailure("consumed handle freed twice") })
        let cancel = Task.detached { handle.cancel() }
        await entered.wait()
        let handoff = Task.detached { handle.handoffToTerminal(); precondition(cancelled.isSet); handed.fire() }
        precondition(!handed.isSet)
        release.signal()
        await cancel.value; await handoff.value
        handle.cancel(); handle.free()
        precondition(handed.isSet)
        print("PASS opt8 terminal handoff joins entered cancel and consumes once")
    }
    for insideTerminal in [true, false] {
        for liftFails in [false, true] {
            let probe = FutureProbe(), entered = Signal()
            let release = DispatchSemaphore(value: 0)
            let worker = Task.detached {
                try await uniffiRustCallAsync(
                    rustFutureFunc: { 1 },
                    pollFunc: { _, callback, data in callback(data, 0) },
                    cancelFunc: { _ in probe.cancel() },
                    completeAndFreeFunc: { _, _ in
                        probe.free()
                        if insideTerminal { entered.fire(); release.wait() }
                        return 42
                    }, freeFunc: { _ in preconditionFailure("consumed native terminal freed twice") },
                    liftFunc: { value in
                        precondition(probe.freeCount == 1)
                        if !insideTerminal { entered.fire(); release.wait() }
                        if liftFails { throw Opt8LiftFailure.expected }
                        return value
                    }, errorHandler: nil)
            }
            await entered.wait()
            // Synchronous cancel must return while native conversion is blocked:
            // holding the lifecycle NSLock over terminal would deadlock here.
            worker.cancel()
            precondition(probe.cancelCount == 0)
            release.signal()
            do { let value = try await worker.value; precondition(!liftFails && value == 42) }
            catch Opt8LiftFailure.expected { precondition(liftFails) }
            precondition(probe.freeCount == 1)
            print("PASS opt8 terminal/lift late cancel preserves committed result and lift failure: native=\(insideTerminal) error=\(liftFails)")
        }
    }
    do {
        let probe = FutureProbe()
        do {
            let _: Int = try await uniffiRustCallAsync(
                rustFutureFunc: { 1 }, pollFunc: { _, callback, data in callback(data, 0) },
                cancelFunc: { _ in probe.cancel() },
                completeAndFreeFunc: { _, status in status.pointee.code = CALL_CANCELLED; probe.free(); return 0 },
                freeFunc: { _ in preconditionFailure("status path double free") },
                liftFunc: { _ in preconditionFailure("status must precede lift") }, errorHandler: nil)
            preconditionFailure("cancel status succeeded")
        } catch is CancellationError {}
        precondition(probe.freeCount == 1)
        print("PASS opt8 cancelled status consumes before host conversion without lift")
    }
    do {
        let entered = Signal(), disposed = Signal(), returned = Signal(), closed = Signal()
        let release = DispatchSemaphore(value: 0)
        let stream = UniFfiStream<Opt7OwnedValue>(rustSubscriptionHandle: 8, next: { _ in
            defer { returned.fire() }
            return .item(try await uniffiRustCallAsync(
                rustFutureFunc: { 1 }, pollFunc: { _, callback, data in callback(data, 0) }, cancelFunc: { _ in },
                completeAndFreeFunc: { _, _ in 42 }, freeFunc: { _ in preconditionFailure("double free") },
                liftFunc: { _ in
                    let value = Opt7OwnedValue { disposed.fire() }
                    entered.fire(); release.wait(); return value
                }, errorHandler: nil))
        }, cancel: { _ in }, close: { _ in
            precondition(returned.isSet && disposed.isSet)
            closed.fire()
        })
        let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
        await entered.wait()
        stream.cancel()
        let closer = Task { try await stream.close() }
        precondition(!returned.isSet && !disposed.isSet && !closed.isSet)
        release.signal()
        do { _ = try await worker.value; preconditionFailure("cancel delivered item") } catch is CancellationError {}
        try await closer.value
        precondition(closed.isSet)
        print("PASS opt8 native free does not ACK host lift/frame or unclaimed value")
    }
    precondition(uniffiContinuationHandleMap.count == 0)
    print("7 opt8 consuming terminal scenarios passed")
}
