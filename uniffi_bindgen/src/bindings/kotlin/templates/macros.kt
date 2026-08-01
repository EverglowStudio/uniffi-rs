{#
// Template to call into rust. Used in several places.
// Variable names in `arg_list` should match up with arg lists
// passed to rust via `arg_list_lowered`
#}

{%- macro to_ffi_call(func) -%}
    {%- match func.self_type() %}
    {%- when Some(Type::Object { .. }) %}
    callWithHandle {
        {%- call to_raw_ffi_call(func) %}{% endcall %}
    }
    {% else %}
        {%- call to_raw_ffi_call(func) %}{% endcall %}
    {% endmatch %}
{%- endmacro %}

{%- macro to_raw_ffi_call(func) -%}
    {%- match func.throws_type() %}
    {%- when Some(e) %}
    {%- if ci.is_external(e) %}
    uniffiRustCallWithError({{ e|type_name(ci) }}ExternalErrorHandler)
    {%- else %}
    uniffiRustCallWithError({{ e|type_name(ci) }})
    {%- endif %}
    {%- else %}
    uniffiRustCall()
    {%- endmatch %} { _status ->
    UniffiLib.{{ func.ffi_func().name() }}(
    {%- match func.self_type() %}
    {%- when Some(Type::Object { .. }) %}
        it,
    {%- when Some(t) %}
        {{- t|lower_fn }}(this),
    {%- when None %}
    {% endmatch %}
        {% call arg_list_lowered(func) %}{% endcall -%}
        _status)
}
{%- endmacro -%}

{%- macro func_decl(func_decl, callable, indent) %}
    {%- call docstring(callable, indent) %}{% endcall %}

    {%- match callable.throws_type() -%}
    {%-     when Some(throwable) %}
    @Throws({{ throwable|type_name(ci) }}::class)
    {%-     else -%}
    {%- endmatch -%}
    {%- if callable.is_async() %}
    @Suppress("ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE")
    {{ func_decl }} suspend fun {{ callable.name()|fn_name }}(
        {%- call arg_list(callable, callable.self_type().is_none()) %}{% endcall -%}
    ){% match callable.return_type() %}{% when Some(return_type) %} : {{ return_type|type_name(ci) }}{% when None %}{%- endmatch %} {
        return {% call call_async(callable) %}{% endcall %}
    }
    {%- else -%}
    {{ func_decl }} fun {{ callable.name()|fn_name }}(
        {%- call arg_list(callable, callable.self_type().is_none()) %}{% endcall -%}
    ){%- match callable.return_type() -%}
    {%-         when Some(return_type) -%}
        : {{ return_type|type_name(ci) }} {
            return {{ return_type|lift_fn }}({% call to_ffi_call(callable) %}{% endcall %})
    }
    {%-         when None %}
        = {% call to_ffi_call(callable) %}{% endcall %}
    {%-     endmatch %}
    {% endif %}
{% endmacro %}

{%- macro stream_func_decl(func_decl, callable, indent) %}
    {%- call docstring(callable, indent) %}{% endcall %}

    {%- if let Some(stream_item_type) = callable.stream_item_type() %}
    {%- if let Some(stream_error_type) = callable.stream_error_type() %}
    {%- if let Some(stream_next_ffi_return_type) = callable.stream_next_ffi_return_type() %}
    {{ func_decl }} fun {{ callable.name()|fn_name }}(
        {%- call arg_list(callable, callable.self_type().is_none()) %}{% endcall -%}
    ) : {{ callable.return_type().unwrap()|type_name(ci) }} {
        val __streamConsumed = AtomicBoolean(false)
        return flow {
            if (!__streamConsumed.compareAndSet(false, true)) {
                throw InternalException("UniFFI output streams may only be consumed once")
            }
            val __streamHandle = {% call to_ffi_call(callable) %}{% endcall %}
            try {
                while (true) {
                    val __streamNext = uniffiRustCallAsync(
                        UniffiLib.{{ callable.ffi_stream_next_func() }}(__streamHandle),
                        { future, callback, continuation -> UniffiLib.{{ callable.ffi_stream_next_rust_future_poll(ci) }}(future, callback, continuation) },
                        { future, continuation -> UniffiLib.{{ callable.ffi_stream_next_rust_future_complete(ci) }}(future, continuation) },
                        { future -> UniffiLib.{{ callable.ffi_stream_next_rust_future_free(ci) }}(future) },
                        { __uniffiLiftStreamNext(
                            it,
                            { buffer -> {{ stream_item_type|read_fn }}(buffer) },
                            { buffer -> {{ stream_error_type|read_fn }}(buffer) },
                        ) },
                        UniffiNullRustCallStatusErrorHandler,
                    )
                    when (__streamNext) {
                        is __UniffiStreamNext.Item -> emit(__streamNext.value)
                        __UniffiStreamNext.Done -> break
                        is __UniffiStreamNext.Error -> throw __streamNext.error
                    }
                }
            } finally {
                UniffiLib.{{ callable.ffi_stream_cancel_func() }}(__streamHandle)
            }
        }
    }
    {%- endif %}
    {%- endif %}
    {%- endif %}
{% endmacro %}

{%- macro call_async(callable) -%}
    uniffiRustCallAsync(

{%- match callable.self_type() %}
{%- when Some(Type::Object { .. }) %}
        callWithHandle { uniffiHandle ->
            UniffiLib.{{ callable.ffi_func().name() }}(
                uniffiHandle,
                {% call arg_list_lowered(callable) %}{% endcall %}
            )
        },
{%- when Some(t) %}
        UniffiLib.{{ callable.ffi_func().name() }}(
            {{- t|lower_fn }}(this),
            {% call arg_list_lowered(callable) %}{% endcall %}
        ),
{%- else %}
        UniffiLib.{{ callable.ffi_func().name() }}({% call arg_list_lowered(callable) %}{% endcall %}),
{%- endmatch %}
        {{ callable|async_poll(ci) }},
        {{ callable|async_complete(ci) }},
        {{ callable|async_free(ci) }},
        // lift function
        {%- match callable.return_type() %}
        {%- when Some(return_type) %}
        { {{ return_type|lift_fn }}(it) },
        {%- when None %}
        { Unit },
        {% endmatch %}
        // Error FFI converter
        {%- match callable.throws_type() %}
        {%- when Some(e) %}
        {%- if ci.is_external(e) %}
        {{ e|type_name(ci) }}ExternalErrorHandler,
        {%- else %}
        {{ e|type_name(ci) }}.ErrorHandler,
        {%- endif %}
        {%- when None %}
        UniffiNullRustCallStatusErrorHandler,
        {%- endmatch %}
    )
{%- endmacro %}

{%- macro arg_list_lowered(func) %}
    {%- for arg in func.arguments() %}
        {{ arg|lower_fn_for_arg }}({{ arg.name()|var_name }}),
    {%- endfor %}
{%- endmacro -%}

{#-
// Arglist as used in kotlin declarations of methods, functions and constructors.
// If is_decl, then default values be specified.
// Note the var_name and type_name filters.
-#}

{% macro arg_list(func, is_decl) %}
{%- for arg in func.arguments() -%}
        {{ arg.name()|var_name }}: {{ arg|lower_type_name_for_arg(ci) }}
{%-     if is_decl %}
{%-         match arg.default_value() %}
{%-             when Some(default) %} = {{ default|render_default(arg, ci) }}
{%-             else %}
{%-         endmatch %}
{%-     endif %}
{%-     if !loop.last %}, {% endif -%}
{%- endfor %}
{%- endmacro %}

{#-
// Arglist as used in the UniffiLib function declarations.
// Note unfiltered name but ffi_type_name filters.
-#}
{%- macro arg_list_ffi_decl(func) %}
    {%- for arg in func.arguments() %}
        {{- arg.name()|var_name }}: {{ arg.type_().borrow()|ffi_type_name_for_direct_arg(ci) -}},
    {%- endfor %}
    {%- if func.has_rust_call_status_arg() %}uniffi_out_err: UniffiRustCallStatus, {% endif %}
{%- endmacro -%}

{% macro field_name(field, field_num) %}
{%- if field.name().is_empty() -%}
v{{- field_num -}}
{%- else -%}
{{ field.name()|var_name }}
{%- endif -%}
{%- endmacro %}

{% macro field_name_unquoted(field, field_num) %}
{%- if field.name().is_empty() -%}
v{{- field_num -}}
{%- else -%}
{{ field.name()|var_name|unquote }}
{%- endif -%}
{%- endmacro %}

 // Macro for destroying fields
{%- macro destroy_fields(member) %}
    Disposable.destroy(
    {%- for field in member.fields() %}
        this.{%- call field_name(field, loop.index) %}{% endcall -%}{% if loop.last %}{% else %},{% endif -%}
    {%- endfor %}
    )
{%- endmacro -%}

{%- macro docstring_value(maybe_docstring, indent_spaces) %}
{%- match maybe_docstring %}
{%- when Some(docstring) %}
{{ docstring|docstring(indent_spaces) }}
{%- else %}
{%- endmatch %}
{%- endmacro %}

{%- macro docstring(defn, indent_spaces) %}
{%- call docstring_value(defn.docstring(), indent_spaces) %}{% endcall %}
{%- endmacro %}

// macro for uniffi_trait implementations.
{% macro uniffi_trait_impls(uniffi_trait_methods) %}
{# We have 2 display traits, kotlin has 1. Prefer `Display` but use `Debug` otherwise #}
{%- if let Some(fmt) = uniffi_trait_methods.display_fmt.or(uniffi_trait_methods.debug_fmt.clone()) %}
    // The local Rust `Display`/`Debug` implementation.
    override fun toString(): String {
        return {{ fmt.return_type().unwrap()|lift_fn }}({% call to_ffi_call(fmt) %}{% endcall %})
    }
{%- endif %}
{%- if let Some(eq) = uniffi_trait_methods.eq_eq %}
    // The local Rust `Eq` implementation - only `eq` is used.
    override fun equals(other: Any?): Boolean {
        if (other !is {{ eq.object_name()|class_name(ci) }}) return false
        return {{ eq.return_type().unwrap()|lift_fn }}({% call to_ffi_call(eq) %}{% endcall %})
    }
{%- endif %}
{%- if let Some(hash) = uniffi_trait_methods.hash_hash %}
    // The local Rust `Hash` implementation
    override fun hashCode(): Int {
        return {{ hash.return_type().unwrap()|lift_fn }}({%- call to_ffi_call(hash) %}{% endcall %}).toInt()
    }
{%- endif %}
{%- if let Some(cmp) = uniffi_trait_methods.ord_cmp %}
    // The local Rust `Ord` implementation
    override fun compareTo(other: {{ cmp.object_name()|class_name(ci) }}): Int {
        return {{ cmp.return_type().unwrap()|lift_fn }}({%- call to_ffi_call(cmp) %}{% endcall %}).toInt()
    }
{%- endif %}
{%- endmacro %}
