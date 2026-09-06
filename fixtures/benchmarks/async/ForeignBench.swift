import Foundation
import benchmarks

func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }
func emit(_ record: [String: Any]) {
    let bytes = try! JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
    print(String(decoding: bytes, as: UTF8.self))
}

final class Callback: BenchAsyncCallback {
    let yielding: Bool
    init(_ yielding: Bool) { self.yielding = yielding }
    func call(value: UInt32) async -> UInt32 {
        if yielding { await Task.yield() }
        return value
    }
}

// Demand-driven source: no AsyncStream buffering or producer task in the workload.
struct Input: AsyncSequence, AsyncIteratorProtocol {
    typealias Element = UInt32
    var position: UInt32 = 0
    let count: UInt32
    mutating func next() async -> UInt32? {
        guard position < count else { return nil }
        defer { position += 1 }
        return position
    }
    func makeAsyncIterator() -> Input { self }
}

@main struct ForeignBench {
    static func main() async throws {
        let args = CommandLine.arguments
        let samples = Int(args[1])!, iterations = Int(args[2])!, warmup = Int(args[3])!
        let count = UInt32(args[4])!
        let filter = args[5]
        let initStart = now()
        let ready = try await benchReady(value: 42)
        precondition(ready == 42)
        emit(["kind": "initialization", "backend": "swift", "first_call_ns": now() - initStart])
        let readyCallback = Callback(false), yieldingCallback = Callback(true)

        func measure(_ name: String, items: UInt32 = 1, _ operation: () async throws -> Void) async throws {
            if filter != "all" && name != filter { return }
            for _ in 0..<warmup { try await operation() }
            for sample in 0..<samples {
                var latencies = [UInt64]()
                latencies.reserveCapacity(iterations)
                let start = now()
                for _ in 0..<iterations {
                    let callStart = now()
                    try await operation()
                    latencies.append(now() - callStart)
                }
                let elapsed = now() - start
                emit(["kind": "sample", "backend": "swift", "case": name, "sample": sample,
                      "iterations": iterations, "items_per_operation": items, "elapsed_ns": elapsed,
                      "latency_ns": latencies])
            }
        }
        try await measure("future_ready") {
            let value = try await benchReady(value: 42)
            precondition(value == 42)
        }
        try await measure("future_pending_wake") {
            let value = try await benchPending(value: 43)
            precondition(value == 43)
        }
        try await measure("callback_ready") {
            let value = try await benchCallback(callback: readyCallback, value: 44)
            precondition(value == 44)
        }
        try await measure("callback_yield") {
            let value = try await benchCallback(callback: yieldingCallback, value: 45)
            precondition(value == 45)
        }
        try await measure("future_cancel_lifecycle") {
            let probe = BenchCancelProbe()
            let task = Task { try await benchCancel(probe: probe) }
            let deadline = now() + 10_000_000_000
            while !probe.isPolled() {
                precondition(now() < deadline, "cancel setup timed out")
                await Task.yield()
            }
            task.cancel()
            do { _ = try await task.value; preconditionFailure("cancel returned a value") }
            catch is CancellationError { }
            while !probe.isDropped() {
                precondition(now() < deadline, "cancel drop timed out")
                await Task.yield()
            }
        }
        for pending in [false, true] {
            try await measure(pending ? "stream_output_pending" : "stream_output_ready", items: count) {
                var index: UInt32 = 0
                for try await value in benchOutputStream(count: count, pending: pending) {
                    precondition(value == index)
                    index += 1
                }
                precondition(index == count)
            }
        }
        let expectedSum = UInt64(count) * UInt64(count - 1) / 2
        try await measure("stream_input", items: count) {
            let sum = try await benchInputStream(events: Input(count: count))
            precondition(sum == expectedSum)
        }
        try await measure("stream_roundtrip", items: count) {
            var index: UInt32 = 0
            for try await value in benchRoundtripStream(events: Input(count: count)) {
                precondition(value == index)
                index += 1
            }
            precondition(index == count)
        }
        benchShutdownWakeWorker()
    }
}
