package uniffi

import uniffi.nullable_custom.CustomText

private fun checkUnpublished(completion: TestCustomCompletion) {
    val error = runCatching { completion.getValue() }.exceptionOrNull()
    check(error is IllegalStateException && error.message == "Rust future completion was not published")
}

fun main() {
    val completion = TestCustomCompletion()
    checkUnpublished(completion)
    check(testGeneratedCustomLift("") == null)
    completion.complete("")
    println("nullable Custom: real generated lift returned null; completion callback returned normally; reading getter")
    val value: CustomText = completion.getValue()
    check(value == null)

    val nonNull = TestCustomCompletion()
    nonNull.complete("hello")
    val nonNullValue: CustomText = nonNull.getValue()
    check(nonNullValue == "hello")

    val failed = TestCustomCompletion()
    check(runCatching { failed.complete(null) }.exceptionOrNull() is NullPointerException)
    checkUnpublished(failed)
    println("nullable Custom completion: legal null/non-null, typed getter, missing and original lift failure passed")
}
