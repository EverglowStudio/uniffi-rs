{%- for arg in func.input_stream_arguments() %}
private fun {{ func.input_stream_initialization_fn_name(arg) }}(lib: UniffiLib) {
    lib.{{ func.ffi_input_stream_init_func(arg) }}(
        uniffiInputStreamNextCallbackImpl,
        uniffiInputStreamCancelCallbackImpl,
    )
}

{%- endfor %}
{%- if func.is_stream() %}
{%- call kt::stream_func_decl("", func, 8) %}{% endcall %}
{%- else %}
{%- call kt::func_decl("", func, 8) %}{% endcall %}
{%- endif %}
