package uniffi

object JniRuntimeProbe {
    @JvmStatic external fun completionLiftFailure(completion: CompleteRustFuture11)
    @JvmStatic external fun conversionFailureNew(): Long
    @JvmStatic external fun receiverNew(mode: Int): Long
    @JvmStatic external fun receiverPoll(handle: Long, id: Long): Boolean
    @JvmStatic external fun receiverCancel(handle: Long)
    @JvmStatic external fun receiverComplete(handle: Long): Int
    @JvmStatic external fun receiverFree(handle: Long)
    @JvmStatic external fun receiverDropped(handle: Long): Boolean
    @JvmStatic external fun receiverWake(handle: Long, phase: Int): Boolean
    @JvmStatic external fun checkAttachment(): Boolean
}
