object Scaffolding {
    @JvmStatic external fun futureNotificationTake(wait: Boolean): Long
    @JvmStatic external fun kotlinCallbackReleased(handle: Long)
    @JvmStatic external fun ffiBufferCheckSupport()
    @JvmStatic external fun ffiBufferAlloc(capacity: kotlin.Int): java.nio.ByteBuffer
    @JvmStatic external fun ffiBufferFree(buf: java.nio.ByteBuffer)
    @JvmStatic external fun ffiBufferReadString(buf: java.nio.ByteBuffer, offset: kotlin.Int): kotlin.String
    @JvmStatic external fun ffiBufferWriteString(buf: java.nio.ByteBuffer, offset: kotlin.Int, value: kotlin.String)
    @JvmStatic external fun ffiBufferReadBytes(buf: java.nio.ByteBuffer, offset: kotlin.Int): kotlin.ByteArray
    @JvmStatic external fun ffiBufferWriteBytes(buf: java.nio.ByteBuffer, offset: kotlin.Int, value: kotlin.ByteArray)
    @JvmStatic external fun ffiBufferReadBuffer(buf: java.nio.ByteBuffer, offset: kotlin.Int): java.nio.ByteBuffer
    @JvmStatic external fun ffiBufferWriteBuffer(buf: java.nio.ByteBuffer, offset: kotlin.Int, value: java.nio.ByteBuffer)

    {%- for (jni_method_name, _, callable) in root.jni_methods() %}
    @JvmName("{{ jni_method_name }}")
    @JvmStatic external fun {{ jni_method_name }}(
        {%- for ffi_arg in callable.ffi_arguments_including_receiver() %}
        {{ ffi_arg.name_kt() }}: {{ ffi_arg.ty.type_kt() }},
        {%- endfor %}
    )
    {%- if callable.is_async %} : kotlin.Long
    {%- else %}
    {%- match callable.return_ffi() %}
    {%- when ReturnFfi::Primitive { ffi_type, .. } %} : {{ ffi_type.type_kt() }}
    {%- when ReturnFfi::Deconstruct { type_node, .. } %} : {{ type_node.type_kt }}
    {%- when ReturnFfi::Void %}
    {%- endmatch %}
    {%- endif %}
    {%- endfor %}

    {%- for cls in root.classes() %}
    @JvmStatic external fun {{ cls.jni_clone_name() }}(handle: kotlin.Long): kotlin.Long
    @JvmStatic external fun {{ cls.jni_free_name() }}(handle: kotlin.Long)
    {%- endfor  %}

    {%- for box_ in root.boxes() %}
    @JvmStatic external fun {{ box_.jni_from_ffi_values_name() }}(
        {%- for ffi_type in box_.inner.ffi_types %}
        v{{ loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
    ): kotlin.Long
    @JvmStatic external fun {{ box_.jni_into_inner_name() }}(handle: kotlin.Long): {{ box_.inner.type_kt }}
    {%- endfor  %}

    {%- for result in root.kotlin_sync_callable_results() %}
    {%- if let Some(return_type) = result.return_type %}
    {%- if !return_type.lowers_to_primitive() %}
    @JvmStatic external fun {{ result.set_callback_return_fn() }}(
        resultPtr: kotlin.Long,
        {%- for ffi_type in return_type.ffi_types %}
        v{{ loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
    )
    {%- endif %}
    {%- endif %}

    {%- if let Some(throws_type) = result.throws_type %}
    @JvmStatic external fun {{ result.set_callback_err_fn() }}(
        resultPtr: kotlin.Long,
        {%- for ffi_type in throws_type.ffi_types %}
        v{{ loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
    )
    {%- endif %}
    {%- endfor %}

    {%- for rust_result in root.rust_async_callable_results() %}
    @JvmStatic external fun {{ rust_result.async_poll_fn() }}(
        rustFuture: kotlin.Long,
        notificationId: kotlin.Long,
    ): kotlin.Boolean
    @JvmStatic external fun {{ rust_result.async_complete_fn() }}(
        rustFuture: kotlin.Long,
        {%- if rust_result.return_type.is_some() %}
        completion: {{ rust_result.async_complete_class() }},
        {%- endif %}
    ): Int
    @JvmStatic external fun {{ rust_result.async_cancel_fn() }}(rustFuture: kotlin.Long)
    @JvmStatic external fun {{ rust_result.async_free_fn() }}(rustFuture: kotlin.Long)
    {%- endfor %}

    {%- for result in root.kotlin_async_callable_results() %}
    @JvmStatic external fun {{ result.async_complete_success_fn() }}(
        oneshotHandle: kotlin.Long,
        {%- if let Some(return_type) = result.return_type %}
        {%- for ffi_type in return_type.ffi_types %}
        v{{ loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
        {%- endif %}
    )
    {%- if let Some(throws_type) = result.throws_type %}
    @JvmStatic external fun {{ result.async_complete_error_fn() }}(
        oneshotHandle: kotlin.Long,
        {%- for ffi_type in throws_type.ffi_types %}
        v{{ loop.index0 }}: {{ ffi_type.type_kt() }},
        {%- endfor %}
    )
    {%- endif %}
    @JvmStatic external fun {{ result.async_complete_unexpected_error_fn() }}(
        oneshotHandle: kotlin.Long,
    )
    {%- endfor %}

    @JvmStatic external fun inputCloseFinished(key: Long, success: Boolean)
    {% for stream in root.streams() %}
    {% let id = stream.self_type.id %}
    @JvmStatic external fun streamDiscard{{ id }}(handle: Long)
    @JvmStatic external fun streamNext{{ id }}(handle: Long): Long
    @JvmStatic external fun streamClose{{ id }}(handle: Long): Long
    {% for suffix in ["Next", "Close"] %}
    @JvmStatic external fun stream{{ suffix }}Poll{{ id }}(handle: Long, notificationId: Long): Boolean
    @JvmStatic external fun stream{{ suffix }}Cancel{{ id }}(handle: Long)
    @JvmStatic external fun stream{{ suffix }}Free{{ id }}(handle: Long)
    {% endfor %}
    @JvmStatic external fun streamNextComplete{{ id }}(handle: Long, completion: StreamComplete{{ id }}): Int
    @JvmStatic external fun streamCloseComplete{{ id }}(handle: Long)
    @JvmStatic external fun inputItem{{ id }}(key: Long, {% for ffi in stream.item.ffi_types %}v{{ loop.index0 }}: {{ ffi.type_kt() }},{% endfor %})
    {% if let Some(error) = stream.error %}
    @JvmStatic external fun inputError{{ id }}(key: Long, {% for ffi in error.ffi_types %}v{{ loop.index0 }}: {{ ffi.type_kt() }},{% endfor %})
    {% endif %}
    @JvmStatic external fun inputDone{{ id }}(key: Long)
    @JvmStatic external fun inputFailed{{ id }}(key: Long)
    {% endfor %}

    init {
        System.loadLibrary("{{ cdylib }}")
        Scaffolding.ffiBufferCheckSupport()
    }
}
