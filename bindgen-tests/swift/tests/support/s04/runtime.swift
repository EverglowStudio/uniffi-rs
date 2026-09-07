@main struct Main {
    static func main() async {
        // Sticky cancellation and a close join that starts before Task publication.
        let box = UniffiInputStreamNextTaskBox()
        box.cancel()
        let release = Signal()
        let joined = Signal()
        let joiner = Task { await box.wait(); joined.fire() }
        await Task.yield()
        precondition(!joined.isSet)
        let task = Task { await release.wait() }
        box.set(task: task)
        precondition(task.isCancelled)
        precondition(!joined.isSet)
        release.fire()
        await joiner.value
        print("PASS sticky cancel and join before task set")

        for dropNext in [false, true] {
            let probe = Probe()
            let handle = create(probe)
            let nextAck = Signal()
            let nextDrop = next(handle) { _, _ in nextAck.fire() }
            await probe.entered.wait()
            if dropNext { nextDrop.free(nextDrop.handle) }
            uniffiInputStreamCancelCallback(handle: handle)
            await probe.cancelled.wait()
            precondition((try? UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle)) != nil)
            let ack = Signal()
            let drop = close(handle, probe, ack)
            drop.free(drop.handle)
            let ack2 = Signal()
            _ = close(handle, probe, ack2)
            for _ in 0..<20 { await Task.yield() }
            precondition(!ack.isSet && !ack2.isSet)
            probe.release.fire()
            await ack.wait()
            await ack2.wait()
            precondition(nextAck.isSet)
            precondition((try? UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle)) == nil)
            print("PASS in-flight cleanup, concurrent close and dropped waiter (next drop=\(dropNext))")
        }

        // Two accepted next Tasks: the newest completing is not sufficient.
        do {
            let probe = Probe(); probe.immediate = true
            let handle = create(probe)
            let firstEntered = Signal()
            let callbackRelease = DispatchSemaphore(value: 0)
            _ = next(handle) { _, _ in
                firstEntered.fire()
                callbackRelease.wait()
            }
            await firstEntered.wait()
            let secondAck = Signal()
            _ = next(handle) { _, _ in secondAck.fire() }
            await secondAck.wait()
            let ack = Signal()
            _ = close(handle, probe, ack)
            for _ in 0..<20 { await Task.yield() }
            precondition(!ack.isSet)
            callbackRelease.signal()
            await ack.wait()
            print("PASS close joins all accepted next tasks and callbacks")
        }

        // Idle iterator destruction itself must be included in every close waiter.
        do {
            let probe = Probe(); probe.blockDeinit = true
            let handle = create(probe)
            uniffiInputStreamCancelCallback(handle: handle)
            precondition(!probe.destroyed.isSet)
            let ack = Signal(); _ = close(handle, probe, ack)
            await probe.deinitEntered.wait()
            let ack2 = Signal(); let drop = close(handle, probe, ack2)
            drop.free(drop.handle)
            for _ in 0..<20 { await Task.yield() }
            precondition(!ack.isSet && !ack2.isSet)
            probe.deinitRelease.signal()
            await ack.wait(); await ack2.wait()
            print("PASS idle close waits for actual deinit, including concurrent waiter")
        }

        for fails in [false, true] {
            let probe = Probe(); probe.fails = fails
            let handle = create(probe)
            let nextAck = Signal()
            _ = next(handle) { _, result in
                precondition(result.callStatus.code == (fails ? CALL_ERROR : 0))
                nextAck.fire()
            }
            probe.release.fire()
            await nextAck.wait()
            let ack = Signal(); _ = close(handle, probe, ack)
            await ack.wait()
            print("PASS terminal close (error=\(fails))")
        }

        do {
            let source = Probe()
            let stream = UniFfiStream(Sequence(probe: source))
            let worker = Task { var iterator = stream.makeAsyncIterator(); return try await iterator.next() }
            await source.entered.wait()
            worker.cancel()
            await source.cancelled.wait()
            let closed = Signal()
            let closeWorker = Task { try await stream.close(); closed.fire() }
            closeWorker.cancel()
            for _ in 0..<20 { await Task.yield() }
            precondition(!closed.isSet)
            source.release.fire()
            do { _ = try await worker.value; preconditionFailure("expected cancellation") }
            catch is CancellationError {}
            catch { preconditionFailure("unexpected error") }
            try! await closeWorker.value
            precondition(source.destroyed.isSet)
            print("PASS output stream cancellation waits for producer and survives close waiter drop")
        }
        do {
            let source = Probe()
            let stream = UniFfiStream(Sequence(probe: source))
            try! await stream.close()
            precondition(source.destroyed.isSet)
            print("PASS idle host stream close releases iterator")
        }
        do {
            let source = Probe()
            let stream = UniFfiStream(Sequence(probe: source))
            let handle = uniffiCreateInputStream(stream, lowerNext: { RustBuffer(value: $0 ?? -1) }, lowerError: { _ in nil })
            let ack = Signal()
            let drop = close(handle, source, ack)
            drop.free(drop.handle)
            await ack.wait()
            precondition(source.destroyed.isSet)
            print("PASS lowering UniFfiStream preserves awaited underlying cleanup")
        }

        do {
            let values = StreamSteps<Int?>([.item(1), .item(nil), .item(2), .done])
            let closed = Signal()
            let stream = UniFfiStream<Int?>(handle: 1, next: { _ in values.next() }, cancel: { _ in }, close: { _ in closed.fire() })
            precondition(values.count == 0)
            var iterator = stream.makeAsyncIterator()
            var rejected = stream.makeAsyncIterator()
            do { _ = try await rejected.next(); preconditionFailure("second consumer accepted") }
            catch UniffiInternalError.streamConsumed {}
            catch { preconditionFailure("wrong error") }
            precondition(values.count == 0)
            let first = try! await iterator.next()
            precondition(first! == 1)
            for _ in 0..<20 { await Task.yield() }
            precondition(values.count == 1)
            guard case .some(.none) = try! await iterator.next() else { preconditionFailure("Item(None) lost") }
            let third = try! await iterator.next()
            precondition(third! == 2)
            let done = try! await iterator.next()
            precondition(done == nil && closed.isSet)
            let again = try! await iterator.next()
            precondition(again == nil && values.count == 4)
            print("PASS optional items, single consumer, no prefetch and terminal close")
        }
        do {
            let source = Probe()
            let stream = UniFfiStream(Sequence(probe: source))
            let iterator = stream.makeAsyncIterator()
            let first = Task { var first = iterator; return try await first.next() }
            await source.entered.wait()
            var second = iterator
            do { _ = try await second.next(); preconditionFailure("concurrent next accepted") }
            catch UniffiInternalError.concurrentStreamNext {}
            catch { preconditionFailure("wrong error") }
            source.release.fire()
            let done = try! await first.value
            precondition(done == nil)
            print("PASS copied iterators reject concurrent next")
        }
        do {
            let closed = Signal()
            let stream = UniFfiStream<Int>(handle: 1, next: { _ in .item(1) }, cancel: { _ in }, close: { _ in closed.fire() })
            try! await consumeOne(stream)
            await closed.wait()
            try! await stream.close()
            print("PASS early for-await break initiates cleanup")
        }
        do {
            let closed = Signal()
            let stream = UniFfiStream<Int>(handle: 1, next: { _ in .error(UniffiInternalError.unexpectedStaleHandle) }, cancel: { _ in }, close: { _ in closed.fire() })
            var iterator = stream.makeAsyncIterator()
            do { _ = try await iterator.next(); preconditionFailure("expected error") }
            catch UniffiInternalError.unexpectedStaleHandle {}
            catch { preconditionFailure("wrong error") }
            precondition(closed.isSet)
            let done = try! await iterator.next()
            precondition(done == nil)
            print("PASS first business error is terminal and awaits cleanup")
        }
        do {
            let stream = UniFfiStream<Int>(handle: 1, next: { _ in .done }, cancel: { _ in }, close: { _ in throw UniffiInternalError.unexpectedStaleHandle })
            for _ in 0..<2 {
                do { try await stream.close(); preconditionFailure("cleanup failure became success") }
                catch UniffiInternalError.unexpectedStaleHandle {}
                catch { preconditionFailure("wrong error") }
            }
            print("PASS repeated close shares cleanup failure")
        }
        do {
            let stream = UniFfiStream<Int>(handle: 1, next: { _ in .done }, cancel: { _ in }, close: { _ in throw UniffiInternalError.unexpectedStaleHandle })
            let handle = uniffiCreateInputStream(stream, lowerNext: { _ in RustBuffer() }, lowerError: { _ in nil })
            let acknowledged = Signal()
            var dropped = UniffiForeignFutureDroppedCallbackStruct()
            uniffiInputStreamCloseCallback(handle: handle, callback: { _, result in
                precondition(result.callStatus.code == CALL_UNEXPECTED_ERROR)
                precondition((try? UNIFFI_INPUT_STREAM_HANDLE_MAP.get(handle: handle)) == nil)
                acknowledged.fire()
            }, callbackData: 0, droppedCallback: &dropped)
            await acknowledged.wait()
            print("PASS input close forwards cleanup failure and releases its registration")
        }
        do {
            let probe = FutureProbe()
            let settled = Signal()
            let task = Task {
                defer { settled.fire() }
                return try await uniffiRustCallAsync(
                    rustFutureFunc: { 7 },
                    pollFunc: { _, callback, data in probe.poll(callback, data) },
                    cancelFunc: { _ in probe.cancel() },
                    completeFunc: { _, status in status.pointee.code = CALL_CANCELLED; return 0 },
                    freeFunc: { _ in probe.free() },
                    liftFunc: { $0 }, errorHandler: nil
                )
            }
            await probe.entered.wait()
            task.cancel()
            await probe.repolled.wait()
            precondition(!settled.isSet && probe.freeCount == 0)
            probe.signal(0)
            do { _ = try await task.value; preconditionFailure("expected cancelled Future") }
            catch is CancellationError {}
            catch { preconditionFailure("wrong error") }
            precondition(probe.freeCount == 1 && probe.cancelCount == 1)
            print("PASS direct Rust Future polls cancellation cleanup before complete/free")
        }

        do {
            let entered = Signal()
            let release = Signal()
            let frameReleased = Signal()
            let valueReleased = Signal()
            let completed = Signal()
            var dropped = UniffiForeignFutureDroppedCallbackStruct()
            uniffiTraitInterfaceCallAsync(makeCall: {
                let frame = HostValue(frameReleased)
                entered.fire()
                await release.wait()
                withExtendedLifetime(frame) {}
                return HostValue(valueReleased)
            }, handleSuccess: { _ in preconditionFailure("cancelled value lowered") }, handleError: { code, _ in
                precondition(code == CALL_CANCELLED)
                precondition(frameReleased.isSet && valueReleased.isSet)
                precondition(!completed.isSet)
                completed.fire()
            }, droppedCallback: &dropped)
            await entered.wait()
            dropped.free(dropped.handle)
            for _ in 0..<20 { await Task.yield() }
            precondition(!completed.isSet)
            release.fire()
            await completed.wait()
            precondition(UNIFFI_FOREIGN_FUTURE_HANDLE_MAP.count == 0)
            print("PASS dropped callback Task waits makeCall frame and discards unclaimed value before one ack")
        }
        do {
            let entered = Signal()
            let release = Signal()
            let source = Probe(); source.blockDeinit = true
            let completed = Signal()
            var dropped = UniffiForeignFutureDroppedCallbackStruct()
            uniffiTraitInterfaceCallAsync(makeCall: {
                entered.fire()
                await release.wait()
                return UniFfiStream(Sequence(probe: source))
            }, handleSuccess: { _ in preconditionFailure("cancelled factory result lowered") }, handleError: { code, _ in
                precondition(code == CALL_CANCELLED && source.destroyed.isSet)
                precondition(!completed.isSet)
                completed.fire()
            }, droppedCallback: &dropped, discardResult: { try? await $0.close() })
            await entered.wait()
            dropped.free(dropped.handle)
            release.fire()
            await source.deinitEntered.wait()
            precondition(!completed.isSet)
            source.deinitRelease.signal()
            await completed.wait()
            print("PASS cancelled host factory waits unclaimed stream close before completion")
        }
        let missing = Signal()
        var dropped = UniffiForeignFutureDroppedCallbackStruct()
        uniffiInputStreamCloseCallback(handle: UInt64.max, callback: { _, result in
            precondition(result.callStatus.code == CALL_UNEXPECTED_ERROR)
            missing.fire()
        }, callbackData: 0, droppedCallback: &dropped)
        await missing.wait()
        precondition(UNIFFI_INPUT_STREAM_HANDLE_MAP.count == 0)
        precondition(UNIFFI_INPUT_STREAM_NEXT_TASK_HANDLE_MAP.count == 0)
        print("PASS missing handle fails and handle maps drain")
        print("20/20 Swift Future/callback/input/output runtime scenarios passed")
    }
}
