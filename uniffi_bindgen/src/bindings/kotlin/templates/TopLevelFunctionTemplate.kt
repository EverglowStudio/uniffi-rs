{%- if func.is_stream() %}
{%- call kt::stream_func_decl("", func, 8) %}{% endcall %}
{%- else %}
{%- call kt::func_decl("", func, 8) %}{% endcall %}
{%- endif %}
