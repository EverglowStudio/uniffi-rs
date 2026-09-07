import Foundation

/// Test state shared with callbacks invoked on Rust worker threads.
public final class TestLocked<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Value
    public init(_ value: Value) { self.value = value }
    public func read() -> Value { lock.withLock { value } }
    public func update(_ body: (inout Value) -> Void) {
        lock.withLock { body(&value) }
    }
}
