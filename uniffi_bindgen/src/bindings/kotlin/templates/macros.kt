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
    {%- if !func.input_stream_arguments().is_empty() %}
    uniffiWithInputStreamArguments { __inputArguments ->
        {% call prepare_input_arguments(func) %}{% endcall %}
    {%- endif %}
    {% call transfer_input_arguments(func) %}{% endcall %}
    UniffiLib.{{ func.ffi_func().name() }}(
    {%- match func.self_type() %}
    {%- when Some(Type::Object { .. }) %}
        it,
    {%- when Some(t) %}
        {{- t|lower_fn }}(this),
    {%- when None %}
    {% endmatch %}
        {% call prepared_arg_list(func) %}{% endcall -%}
        _status)
    {%- if !func.input_stream_arguments().is_empty() %}
    }
    {%- endif %}
}
{%- endmacro -%}

{%- macro func_decl(func_decl, callable, indent) %}
    {%- call docstring(callable, indent) %}{% endcall %}

    {%- match callable.throws_type() -%}
    {%-     when Some(throwable) %}
    @Throws({{ throwable|type_name(ci) }}::class)
    {%-     else -%}
    {%- endmatch -%}
    {%- if callable.is_stream() %}
    {%- call stream_func_decl(func_decl, callable, indent) %}{% endcall %}
    {%- else if callable.is_async() %}
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
    {{ func_decl }} {% if callable.is_async() %}suspend {% endif %}fun {{ callable.name()|fn_name }}(
        {%- call arg_list(callable, callable.self_type().is_none()) %}{% endcall -%}
    ) : {{ callable.return_type().unwrap()|type_name(ci) }} {
        {%- if callable.is_async() %}
        return {% call call_async(callable) %}{% endcall %}
        {%- else %}
        val __streamHandle = {% call to_ffi_call(callable) %}{% endcall %}
        return {% call lift_stream(callable, "__streamHandle") %}{% endcall %}
        {%- endif %}
    }
{% endmacro %}

{%- macro lift_stream(callable, handle) -%}
    UniFfiStream.rustSubscription(
        next = {
            uniffiRustCallAsync(
                UniffiLib.{{ callable.ffi_stream_next_func() }}({{ handle }}),
                { future, callback, continuation -> UniffiLib.{{ callable.ffi_stream_next_rust_future_poll(ci) }}(future, callback, continuation) },
                { future -> UniffiLib.{{ callable.ffi_stream_next_rust_future_cancel(ci) }}(future) },
                { future, status -> UniffiLib.{{ callable.ffi_stream_next_rust_future_complete_and_free(ci) }}(future, status) },
                { future -> UniffiLib.{{ callable.ffi_stream_next_rust_future_free(ci) }}(future) },
                { __uniffiLiftStreamNext(
                    it,
                    { buffer -> {{ callable.stream_item_type().unwrap()|read_fn }}(buffer) },
                    {%- if let Some(error_type) = callable.stream_error_type() %}
                    { buffer -> {{ error_type|read_fn }}(buffer) },
                    {%- else %}
                    { _ -> throw InternalException("infallible stream returned a business error") },
                    {%- endif %}
                ) },
                UniffiNullRustCallStatusErrorHandler,
            )
        },
        closeSubscription = {
            uniffiRustCallAsync(
                UniffiLib.{{ callable.ffi_stream_close_func() }}({{ handle }}),
                { future, callback, continuation -> UniffiLib.{{ callable.ffi_stream_close_rust_future_poll(ci) }}(future, callback, continuation) },
                { future -> UniffiLib.{{ callable.ffi_stream_close_rust_future_cancel(ci) }}(future) },
                { future, status -> UniffiLib.{{ callable.ffi_stream_close_rust_future_complete_and_free(ci) }}(future, status) },
                { future -> UniffiLib.{{ callable.ffi_stream_close_rust_future_free(ci) }}(future) },
                {},
                UniffiNullRustCallStatusErrorHandler,
            )
        },
    )
{%- endmacro %}

{%- macro call_async(callable) -%}
    uniffiRustCallAsync(

        {%- if !callable.input_stream_arguments().is_empty() %}
        uniffiWithInputStreamArguments { __inputArguments ->
            {% call prepare_input_arguments(callable) %}{% endcall %}
        {%- endif %}
        {%- match callable.self_type() %}
        {%- when Some(Type::Object { .. }) %}
        callWithHandle { uniffiHandle ->
            {% call transfer_input_arguments(callable) %}{% endcall %}
            UniffiLib.{{ callable.ffi_func().name() }}(
                uniffiHandle,
                {% call prepared_arg_list(callable) %}{% endcall %}
            )
        }
        {%- when Some(t) %}
        {% call transfer_input_arguments(callable) %}{% endcall %}
        UniffiLib.{{ callable.ffi_func().name() }}(
            {{- t|lower_fn }}(this),
            {% call prepared_arg_list(callable) %}{% endcall %}
        )
        {%- else %}
        {% call transfer_input_arguments(callable) %}{% endcall %}
        UniffiLib.{{ callable.ffi_func().name() }}({% call prepared_arg_list(callable) %}{% endcall %})
        {%- endmatch %}
        {%- if !callable.input_stream_arguments().is_empty() %}
        }
        {%- endif %},
        {{ callable|async_poll(ci) }},
        {{ callable|async_cancel(ci) }},
        {{ callable|async_complete(ci) }},
        {{ callable|async_free(ci) }},
        // Lift only after Rust commits completion; a cancelled factory owns its result.
        {%- if callable.is_stream() %}
        { __streamHandle -> {% call lift_stream(callable, "__streamHandle") %}{% endcall %} },
        {%- else %}
        {%- match callable.return_type() %}
        {%- when Some(return_type) %}
        { {{ return_type|lift_fn }}(it) },
        {%- when None %}
        {},
        {% endmatch %}
        {%- endif %}
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
        {%- if callable.is_stream() %}
        discardResult = { it.close() },
        {%- endif %}
    )
{%- endmacro %}

{%- macro transfer_input_arguments(func) %}
    {%- if !func.input_stream_arguments().is_empty() %}
    __inputArguments.transfer()
    {%- endif %}
{%- endmacro %}

{%- macro prepare_input_arguments(func) %}
    {%- for arg in func.arguments() %}
    val __uniffiArg{{ loop.index }} = {{ arg|lower_fn_for_arg }}({{ arg.name()|var_name }}){% if arg|is_input_stream_arg %}.also { __inputArguments.add(it) }{% endif %}
    {%- endfor %}
{%- endmacro %}

{%- macro prepared_arg_list(func) %}
    {%- if func.input_stream_arguments().is_empty() %}
    {% call arg_list_lowered(func) %}{% endcall %}
    {%- else %}
    {%- for arg in func.arguments() %}
    __uniffiArg{{ loop.index }},
    {%- endfor %}
    {%- endif %}
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

 // Macro for destroying fields
{%- macro destroy_fields(member) %}
    Disposable.destroy(
    {%- for field in member.fields() %}
        this.{%- call field_name(field, loop.index) %}{% endcall -%}{% if loop.last %}{% else %},{% endif -%}
    {%- endfor %}
    )
{%- endmacro -%}

{%- macro destroy_error_fields(member) %}
    Disposable.destroy(
    {%- for field in member.fields() %}
        this.{{ field|error_field_name(member, loop.index) }}{% if loop.last %}{% else %},{% endif -%}
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
