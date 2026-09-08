// Test-only ABI adapter for the unchanged S04/stream gate bodies. The production
// driver only accepts the consuming terminal; these deterministic native stubs
// still supply complete/free separately so the adapter performs their native order.
fileprivate func uniffiRustCallAsync<F, T: Sendable>(
    rustFutureFunc: @Sendable () -> UInt64,
    pollFunc: @Sendable (UInt64, @escaping UniffiRustFutureContinuationCallback, UInt64) -> (),
    cancelFunc: @escaping @Sendable (UInt64) -> (),
    completeFunc: @Sendable (UInt64, UnsafeMutablePointer<RustCallStatus>) -> F,
    freeFunc: @escaping @Sendable (UInt64) -> (),
    liftFunc: @Sendable (F) throws -> T,
    errorHandler: (@Sendable (RustBuffer) throws -> Swift.Error)?
) async throws -> T {
    try await uniffiRustCallAsync(
        rustFutureFunc: rustFutureFunc, pollFunc: pollFunc, cancelFunc: cancelFunc,
        completeAndFreeFunc: { handle, status in
            let value = completeFunc(handle, status)
            freeFunc(handle)
            return value
        }, freeFunc: freeFunc, liftFunc: liftFunc, errorHandler: errorHandler)
}
