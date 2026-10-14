import uniffi.errors_defaults.*

fun checkFailure(error: SdkException) {
    when (error) {
        is SdkException.Failure -> {
            check(error.code == ErrorCode.NETWORK && error.message == "network unavailable")
            check(error.retryable && error.messageId == "m-1")
        }
        is SdkException.Rejected -> check(error.reason == "invalid")
        is SdkException.Closed -> Unit
    }
}

fun main() {
    for (kind in 0u..2u) {
        try { fail(kind); error("Rust must throw") }
        catch (error: SdkException) { checkFailure(error) }
    }
    try { failUnit(); error("Rust must throw unit") }
    catch (error: UnitException) { check(error is UnitException.Offline) }
    val state = errorState()
    checkFailure(state.error)
    checkFailure(requireNotNull(state.optionalError))
    check(isOriginal(state))
    check(isOriginal(echoErrors(state)))
    val constructed = ErrorState(
        SdkException.Failure(ErrorCode.NETWORK, "network unavailable", true, "m-1"),
        SdkException.Closed(), UnitException.Offline(), UnitException.Closed())
    check(isOriginal(constructed))
    val absent = echoErrors(ErrorState(state.error, null, state.unitError, null))
    check(absent.optionalError == null && absent.optionalUnit == null)
    val omitted = inspectDefaults(Defaults(required = "r"))
    val explicitNull = inspectDefaults(Defaults(required = "r", label = null))
    check(omitted == "r|7|record-default|<none>")
    check(explicitNull == "r|7|<none>|<none>")
    check(inspectDefaults(Defaults("r", 2u, "custom", "value")) == "r|2|custom|value")
    check(inspectArguments("r") == "r|11|argument-default|<none>")
    check(inspectArguments("r", label = null) == "r|11|<none>|<none>")
    println("KOTLIN_ERRORS_DEFAULTS_OK $omitted $explicitNull")
}
