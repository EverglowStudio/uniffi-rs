package uniffi

// Controlled lift functions for the real Async template's synthetic typed IR.
private val liftFailure = IllegalArgumentException("completion lift failed")
private var beforeLiftReturn: (() -> Unit)? = null
fun liftUInt(v: Int): UInt = v.toUInt()
fun liftOptionUInt(v: Int): UInt? = if (v < 0) null else v.toUInt()
fun liftType803(v: Int): UInt? = liftOptionUInt(v)
fun liftType804(v: Int): List<String?> = listOf(v.toString(), null)
fun liftType805(v: Int): List<String?>? = if (v < 0) null else liftType804(v)
fun liftType806(v: Int): List<String?>? = liftType805(v)
fun liftString(v: String?): String {
    beforeLiftReturn?.invoke()
    if (v == "throw") throw liftFailure
    return checkNotNull(v)
}
private fun missing(block: () -> Any?) {
    val failure = runCatching(block).exceptionOrNull()
    check(failure is IllegalStateException && failure.message == "Rust future completion was not published")
}
fun main() {
    val scalar = CompleteRustFuture802()
    missing { scalar.getValue() }
    scalar.complete(42)
    val value: UInt = scalar.getValue()
    check(value == 42U)
    for (raw in listOf(-1, 42)) {
        val optional = CompleteRustFuture803()
        missing { optional.getValue() }
        optional.complete(raw)
        val nullableValue: UInt? = optional.getValue()
        check(nullableValue == if (raw < 0) null else 42U)
        val boxed = CompleteRustFuture804()
        missing { boxed.getValue() }
        boxed.complete(raw)
        check(boxed.getValue() == nullableValue)
        val container = CompleteRustFuture806()
        missing { container.getValue() }
        container.complete(raw)
        val nullableContainer: List<String?>? = container.getValue()
        check(nullableContainer == if (raw < 0) null else listOf("42", null))
        val boxedContainer = CompleteRustFuture807()
        missing { boxedContainer.getValue() }
        boxedContainer.complete(raw)
        check(boxedContainer.getValue() == nullableContainer)
    }
    val list = CompleteRustFuture805()
    missing { list.getValue() }
    list.complete(42)
    val nonNullContainer: List<String?> = list.getValue()
    check(nonNullContainer == listOf("42", null))
    val throwing = CompleteRustFuture808()
    beforeLiftReturn = { missing { throwing.getValue() } }
    try {
        check(runCatching { throwing.complete("throw") }.exceptionOrNull() === liftFailure)
    } finally { beforeLiftReturn = null }
    missing { throwing.getValue() }
    val text = CompleteRustFuture808()
    beforeLiftReturn = { missing { text.getValue() } }
    try { text.complete("success") } finally { beforeLiftReturn = null }
    check(text.getValue() == "success")
    println("completion typed gates: scalar, nullable null/value, Box<Optional>, containers, publication order, missing and throwing lift passed")
}
