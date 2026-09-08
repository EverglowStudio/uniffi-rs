{%- let id = stream.self_type.id %}
{%- let item = stream.item %}

fun {{ stream.self_type.lift_fn_kt() }}(handle: Long): kotlinx.coroutines.flow.Flow<{{ item.type_kt }}> = RustStream(
    pull = {
        val completion = StreamComplete{{ id }}()
        uniffiAwaitRustFuture(
            { Scaffolding.streamNext{{ id }}(handle) },
            { future, continuation -> uniffiPollRustFuture(future, continuation, Scaffolding::streamNextPoll{{ id }}) }, Scaffolding::streamNextCancel{{ id }},
            { future ->
                if (Scaffolding.streamNextComplete{{ id }}(future, completion) == UNIFFI_RUST_FUTURE_CANCELLED) {
                    throw kotlin.coroutines.cancellation.CancellationException()
                }
                completion.result!!.getOrThrow()
            },
            Scaffolding::streamNextFree{{ id }},
        )
    },
    discard = { Scaffolding.streamDiscard{{ id }}(handle) },
    release = {
        uniffiAwaitRustFuture(
            { Scaffolding.streamClose{{ id }}(handle) },
            { future, continuation -> uniffiPollRustFuture(future, continuation, Scaffolding::streamClosePoll{{ id }}) }, Scaffolding::streamCloseCancel{{ id }},
            Scaffolding::streamCloseComplete{{ id }}, Scaffolding::streamCloseFree{{ id }},
        )
    },
)

class StreamComplete{{ id }} {
    var result: Result<StreamStep<{{ item.type_kt }}>>? = null
    fun done() { result = Result.success(StreamStep.Done) }
    fun item({% for ffi in item.ffi_types %}v{{ loop.index0 }}: {{ ffi.type_kt() }},{% endfor %}) {
        result = Result.success(StreamStep.Item({{ item.lift_fn_kt() }}({% for _ in item.ffi_types %}v{{ loop.index0 }},{% endfor %})))
    }
    {% if let Some(error) = stream.error %}
    fun error({% for ffi in error.ffi_types %}v{{ loop.index0 }}: {{ ffi.type_kt() }},{% endfor %}) {
        result = Result.failure({{ error.lift_fn_kt() }}({% for _ in error.ffi_types %}v{{ loop.index0 }},{% endfor %}))
    }
    {% endif %}
}

private val uniffiInputs{{ id }} = HandleMap<UniffiInputStream<{{ item.type_kt }}>>()
fun {{ stream.self_type.lower_fn_kt() }}(value: kotlinx.coroutines.flow.Flow<{{ item.type_kt }}>): Long =
    uniffiInputs{{ id }}.insert(UniffiInputStream(value))

fun inputNext{{ id }}(handle: Long, key: Long, released: Long) {
    val input = uniffiInputs{{ id }}.get(handle)
    uniffiLaunchCallback(key, released, { Scaffolding.inputFailed{{ id }}(key) }) delivery@ {
        try {
            when (val step = input.next()) {
                is StreamStep.Item -> return@delivery {
                    val value = {{ item.lower_fn_kt() }}(step.value)
                    Scaffolding.inputItem{{ id }}(key, {% for (var, _) in item.ffi_values_kt("value") %}{{ var }},{% endfor %})
                }
                StreamStep.Done -> return@delivery { Scaffolding.inputDone{{ id }}(key) }
            }
        }
        {% if let Some(error) = stream.error %}
        catch (error: {{ error.type_kt }}) {
            return@delivery {
                val value = {{ error.lower_fn_kt() }}(error)
                Scaffolding.inputError{{ id }}(key, {% for (var, _) in error.ffi_values_kt("value") %}{{ var }},{% endfor %})
            }
        }
        {% endif %}
        catch (error: Throwable) { return@delivery { Scaffolding.inputFailed{{ id }}(key) } }
    }
}
fun inputCancel{{ id }}(handle: Long) { uniffiInputs{{ id }}.get(handle).cancel() }
fun inputClose{{ id }}(handle: Long, key: Long, released: Long) {
    val input = uniffiInputs{{ id }}.get(handle)
    uniffiLaunchCallback(key, released, { Scaffolding.inputCloseFinished(key, false) }) delivery@ {
        try {
            input.close()
            uniffiInputs{{ id }}.remove(handle)
            return@delivery { Scaffolding.inputCloseFinished(key, true) }
        } catch (error: Throwable) { return@delivery { Scaffolding.inputCloseFinished(key, false) } }
    }
}
