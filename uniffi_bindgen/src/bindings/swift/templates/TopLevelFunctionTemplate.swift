{%- if func.is_stream() %}
{%- call swift::stream_func_decl("public func", func, 0) %}{% endcall %}
{%- else %}
{%- call swift::func_decl("public func", func, 0) %}{% endcall %}
{%- endif %}
