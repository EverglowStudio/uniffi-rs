{%- for arg in func.input_stream_arguments() %}
private func {{ func.input_stream_initialization_fn_name(arg) }}() {
    {{ func.ffi_input_stream_init_func(arg) }}(
        uniffiInputStreamNextCallback,
        uniffiInputStreamCancelCallback
    )
}

{%- endfor %}
{%- if func.is_stream() %}
{%- call swift::stream_func_decl("public func", func, 0) %}{% endcall %}
{%- else %}
{%- call swift::func_decl("public func", func, 0) %}{% endcall %}
{%- endif %}
