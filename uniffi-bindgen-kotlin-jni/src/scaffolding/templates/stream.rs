{%- let id = stream.self_type.id %}
{%- let item = stream.item %}
{%- let error_rs = stream.error_rs() %}
{%- let step_rs = "uniffi::UniFfiStreamStep<{}, {}>"|format(item.type_rs, error_rs) %}
static UNIFFI_JNI_STREAM_{{ id }}: uniffi::RustStreamRegistry<{{ item.type_rs }}, {{ error_rs }}> =
    uniffi::deps::once_cell::sync::Lazy::new(Default::default);

unsafe fn {{ stream.self_type.lower_fn_rs() }}(
    _: *mut uniffi_jni::JNIEnv, value: {{ stream.self_type.type_rs }},
) -> uniffi::Result<i64> {
    Ok(uniffi::rust_stream_new(&UNIFFI_JNI_STREAM_{{ id }}, {% if stream.input %}Box::pin(value){% else %}value{% endif %}).as_raw() as i64)
}
unsafe fn {{ stream.self_type.lift_fn_rs() }}(
    _: *mut uniffi_jni::JNIEnv, handle: i64,
) -> uniffi::Result<{{ stream.self_type.type_rs }}> {
    let input = uniffi::ForeignInputStream::new(uniffi::Handle::from_raw_unchecked(handle as u64), UniffiInputOps{{ id }});
    {% if stream.input %}Ok(input){% else %}Ok(Box::pin(input)){% endif %}
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_streamDiscard{{ id }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) {
    uniffi::rust_stream_cancel(&UNIFFI_JNI_STREAM_{{ id }}, uniffi::Handle::from_raw_unchecked(handle as u64));
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_streamNext{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) -> i64 {
    uniffi_jni::rust_call_with_env(env, |_| Ok(uniffi_jni::future_new_with_runtime(
        uniffi::rust_stream_next_async(&UNIFFI_JNI_STREAM_{{ id }}, uniffi::Handle::from_raw_unchecked(handle as u64)),
        uniffi::rust_stream_runtime,
    ).as_raw() as i64))
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_streamClose{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) -> i64 {
    uniffi_jni::rust_call_with_env(env, |_| Ok(uniffi_jni::future_new_with_runtime(async move {
        uniffi::rust_stream_close_async(&UNIFFI_JNI_STREAM_{{ id }}, uniffi::Handle::from_raw_unchecked(handle as u64))
            .await.map_err(|error| uniffi::LiftArgsError { arg_name: "stream close", error: error.into() })
    }, uniffi::rust_stream_runtime).as_raw() as i64))
}

{% for (suffix, output) in stream.future_types() %}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_stream{{ suffix }}Poll{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64, notification_id: i64,
) -> bool {
    uniffi_jni::rust_call_with_env(env, |_| {
        Ok(uniffi_jni::future_poll::<{{ output }}>(
            uniffi::Handle::from_raw_unchecked(handle as u64),
            uniffi_jni::future_notification_enqueue, notification_id as u64,
        ))
    })
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_stream{{ suffix }}Cancel{{ id }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) { uniffi_jni::future_cancel::<{{ output }}>(uniffi::Handle::from_raw_unchecked(handle as u64)); }
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_stream{{ suffix }}Free{{ id }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) { uniffi_jni::future_free::<{{ output }}>(uniffi::Handle::from_raw_unchecked(handle as u64)); }
{% endfor %}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_streamCloseComplete{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) {
    uniffi_jni::rust_call_with_env(env, |_| uniffi_jni::future_complete::<()>(uniffi::Handle::from_raw_unchecked(handle as u64)));
    // Same terminal ownership as ordinary async: free outside the panic catch.
    uniffi_jni::future_free::<()>(uniffi::Handle::from_raw_unchecked(handle as u64));
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_streamNextComplete{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64, completion: uniffi_jni::jobject,
) -> i32 {
    let code = uniffi_jni::rust_call_with_env(env, |env| {
        let step = match uniffi_jni::future_complete::<{{ step_rs }}>(uniffi::Handle::from_raw_unchecked(handle as u64)) {
            Ok(step) => step,
            Err(error) if uniffi_jni::is_cancelled(&error) => return Ok(1),
            Err(error) => return Err(error),
        };
        match step {
            uniffi::UniFfiStreamStep::Done => {
                static DONE: uniffi_jni::CachedMethod = uniffi_jni::CachedMethod::new(c"uniffi/StreamComplete{{ id }}", c"done", c"()V");
                DONE.call_void(env, completion, []).to_anyhow_result(env, "stream done")?;
            }
            uniffi::UniFfiStreamStep::Item(value) => {
                static ITEM: uniffi_jni::CachedMethod = uniffi_jni::CachedMethod::new(c"uniffi/StreamComplete{{ id }}", c"item",
                    c"({% for ty in item.ffi_types %}{{ ty.jni_signature() }}{% endfor %})V");
                let value = {{ item.lower_fn_rs() }}(env, value)?;
                ITEM.call_void(env, completion, [
                    {% for (var, ty) in item.ffi_values_rs("value") %}uniffi_jni::jvalue { {{ ty.jvalue_field() }}: {{ var }} },{% endfor %}
                ]).to_anyhow_result(env, "stream item")?;
            }
            uniffi::UniFfiStreamStep::Error(error) => {
                {% if let Some(error_type) = stream.error %}
                static ERROR: uniffi_jni::CachedMethod = uniffi_jni::CachedMethod::new(c"uniffi/StreamComplete{{ id }}", c"error",
                    c"({% for ty in error_type.ffi_types %}{{ ty.jni_signature() }}{% endfor %})V");
                let error = {{ error_type.lower_fn_rs() }}(env, error)?;
                ERROR.call_void(env, completion, [
                    {% for (var, ty) in error_type.ffi_values_rs("error") %}uniffi_jni::jvalue { {{ ty.jvalue_field() }}: {{ var }} },{% endfor %}
                ]).to_anyhow_result(env, "stream error")?;
                {% else %}match error {}{% endif %}
            }
        }
        Ok(2)
    });
    // The caller has drained this next future and closed cancellation. Preserve
    // the original conversion/exception boundary before consuming its handle.
    uniffi_jni::future_free::<{{ step_rs }}>(uniffi::Handle::from_raw_unchecked(handle as u64));
    code
}

struct UniffiInputOps{{ id }};
impl uniffi::ForeignInputStreamOps<{{ item.type_rs }}, {{ error_rs }}> for UniffiInputOps{{ id }} {
    fn next(&self, handle: uniffi::Handle) -> uniffi::ForeignInputStreamNextFuture<{{ item.type_rs }}, {{ error_rs }}> {
        Box::pin(async move {
            let result = uniffi_kotlin_call(|key, released| unsafe {
                static NEXT: uniffi_jni::CachedStaticMethod = uniffi_jni::CachedStaticMethod::new(c"uniffi/UniffiKt", c"inputNext{{ id }}", c"(JJJ)V");
                uniffi_jni::attach_current_thread(uniffi_get_global_jvm(), |env| NEXT.call_void(env, [
                    uniffi_jni::jvalue { j: handle.as_raw() as i64 }, uniffi_jni::jvalue { j: key }, uniffi_jni::jvalue { j: released },
                ]).to_anyhow_result(env, "input next"))
            }).await;
            result.unwrap_or_else(|error| panic!("input stream failed: {error}"))
        })
    }
    fn cancel(&self, handle: uniffi::Handle) {
        static CANCEL: uniffi_jni::CachedStaticMethod = uniffi_jni::CachedStaticMethod::new(c"uniffi/UniffiKt", c"inputCancel{{ id }}", c"(J)V");
        unsafe { uniffi_jni::attach_current_thread(uniffi_get_global_jvm(), |env| {
            CANCEL.call_void(env, [uniffi_jni::jvalue { j: handle.as_raw() as i64 }]).warn_on_exception(env, "input cancel");
        }); }
    }
    fn close(&self, handle: uniffi::Handle) -> uniffi::ForeignInputStreamCloseFuture {
        Box::pin(async move {
            uniffi_kotlin_call::<()>(|key, released| unsafe {
                static CLOSE: uniffi_jni::CachedStaticMethod = uniffi_jni::CachedStaticMethod::new(c"uniffi/UniffiKt", c"inputClose{{ id }}", c"(JJJ)V");
                uniffi_jni::attach_current_thread(uniffi_get_global_jvm(), |env| CLOSE.call_void(env, [
                    uniffi_jni::jvalue { j: handle.as_raw() as i64 }, uniffi_jni::jvalue { j: key }, uniffi_jni::jvalue { j: released },
                ]).to_anyhow_result(env, "input close"))
            }).await.expect("input stream cleanup failed");
        })
    }
}

{% for (suffix, ty) in stream.input_completions() %}
{% if suffix != "Error" || ty.is_some() %}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_input{{ suffix }}{{ id }}(
    env: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, key: i64,
    {% if let Some(ty) = ty %}{% for ffi in ty.ffi_types %}v{{ loop.index0 }}: {{ ffi.type_rs() }},{% endfor %}{% endif %}
) {
    let sender = uniffi::oneshot::Sender::<uniffi::Result<Result<Option<{{ item.type_rs }}>, {{ error_rs }}>>>::from_raw(std::ptr::with_exposed_provenance(key as usize));
    let result = uniffi_jni::rust_call(|| {
        {% if let Some(ty) = ty %}
        let value = {{ ty.lift_fn_rs() }}(env, {% for _ in ty.ffi_types %}v{{ loop.index0 }},{% endfor %})?;
        {% endif %}
        Ok({% if suffix == "Item" %}Ok(Some(value)){% else if suffix == "Error" %}Err(value){% else %}Ok(None){% endif %})
    });
    sender.send(result.map_err(|error| uniffi::deps::anyhow::anyhow!("{error}")));
}
{% endif %}
{% endfor %}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_inputFailed{{ id }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, key: i64,
) {
    let sender = uniffi::oneshot::Sender::<uniffi::Result<Result<Option<{{ item.type_rs }}>, {{ error_rs }}>>>::from_raw(std::ptr::with_exposed_provenance(key as usize));
    sender.send(Err(uniffi::deps::anyhow::anyhow!("input producer failed")));
}
