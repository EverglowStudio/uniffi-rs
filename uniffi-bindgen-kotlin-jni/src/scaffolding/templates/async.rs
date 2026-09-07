const UNIFFI_RUST_FUTURE_CANCELLED: i32 = 1;
const UNIFFI_RUST_FUTURE_COMPLETE: i32 = 2;

static UNIFFI_JNI_RESUME: uniffi_jni::CachedStaticMethod = uniffi_jni::CachedStaticMethod::new(
        c"uniffi/UniffiKt", c"uniffiContinuationResume", c"(Lkotlin/coroutines/Continuation;Z)V",
);

extern "C" fn uniffi_jni_future_continuation(data: u64, poll: uniffi::RustFuturePoll) {
    // SAFETY: each poll transfers one global reference to core's exactly-once
    // continuation. Delete it before Java can resume/reenter and free the owner.
    unsafe {
        uniffi_jni::attach_current_thread(uniffi_get_global_jvm(), |env| {
            let global = std::ptr::with_exposed_provenance_mut(data as usize);
            let local = ((**env).v1_2.NewLocalRef)(env, global);
            ((**env).v1_2.DeleteGlobalRef)(env, global);
            UNIFFI_JNI_RESUME.call_void(env, [
                uniffi_jni::jvalue { l: local },
                uniffi_jni::jvalue { z: poll == uniffi::RustFuturePoll::Ready },
            ]).warn_on_exception(env, "uniffiContinuationResume");
        });
    }
}

{%- for rust_result in root.rust_async_callable_results() %}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ rust_result.async_poll_fn() }}(
    env: *mut uniffi_jni::JNIEnv,
    _: uniffi_jni::jclass,
    handle: i64,
    continuation: uniffi_jni::jobject,
) {
    uniffi_jni::rust_call_with_env(env, |env| {
        UNIFFI_JNI_RESUME.get(env);
        let continuation = ((**env).v1_2.NewGlobalRef)(env, continuation);
        if continuation.is_null() { uniffi::deps::anyhow::bail!("NewGlobalRef failed"); }
        uniffi_jni::future_poll::<{{ rust_result.return_type_rs() }}>(
            uniffi::Handle::from_raw_unchecked(handle as u64), uniffi_jni_future_continuation,
            continuation.expose_provenance() as u64,
        );
        Ok(())
    });
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ rust_result.async_complete_fn() }}(
    uniffi_env: *mut uniffi_jni::JNIEnv,
    _: uniffi_jni::jclass,
    handle: i64,
    {%- if rust_result.return_type.is_some() %}
    completion: uniffi_jni::jobject,
    {%- endif %}
) -> i32 {
    {%- if let Some(return_type) = rust_result.return_type %}
    static UNIFFI_COMPLETE_METHOD: uniffi_jni::CachedMethod = uniffi_jni::CachedMethod::new(
        c"uniffi/{{ rust_result.async_complete_class() }}", c"complete",
        c"({% for ffi_type in return_type.ffi_types %}{{ ffi_type.jni_signature() }}{% endfor %})V",
    );
    {%- endif %}
    uniffi_jni::rust_call_with_env(uniffi_env, |uniffi_env| {
        let uniffi_return = match uniffi_jni::future_complete::<{{ rust_result.return_type_rs() }}>(
            uniffi::Handle::from_raw_unchecked(handle as u64),
        ) {
            Ok(value) => value,
            Err(error) if uniffi_jni::is_cancelled(&error) => return Ok(UNIFFI_RUST_FUTURE_CANCELLED),
            Err(error) => return Err(error),
        };
                    {%- if let Some(throws_type) = rust_result.throws_type %}
                    let uniffi_return = match uniffi_return {
                        Ok(v) => v,
                        Err(uniffi_err) => {
                            let uniffi_err_lowered = {{ throws_type.lower_fn_rs() }}(uniffi_env, uniffi_err)?;
                            let uniffi_err_obj = {{ throws_type.lift_kt_from_rust_var() }}.call_object(
                                uniffi_env,
                                [
                                    {%- for (var, ffi_type) in throws_type.ffi_values_rs("uniffi_err_lowered") %}
                                    uniffi_jni::jvalue {
                                        {{ ffi_type.jvalue_field() }}: {{ var }},
                                    },
                                    {%- endfor %}
                                ]
                            ).to_anyhow_result(uniffi_env, "{{ throws_type.lift_fn_kt() }}")?;
                            if ((**uniffi_env).v1_4.Throw)(uniffi_env, uniffi_err_obj) != 0 {
                                uniffi::deps::anyhow::bail!("Failed to throw exception for {{ rust_result.async_poll_fn() }}");
                            }
                            // The return value doesn't matter, since the Kotlin code will throw once it's
                            // resumes.  Let's use UNIFFI_RUST_FUTURE_CANCELLED so that if that fails somehow
                            // we the async function will still fail.
                            return Ok(UNIFFI_RUST_FUTURE_CANCELLED);
                        }
                    };
                    {%- endif %}

                    {%- if let Some(return_type) = rust_result.return_type %}
                    let uniffi_return_lowered = {{ return_type.lower_fn_rs() }}(uniffi_env, uniffi_return)?;
                    UNIFFI_COMPLETE_METHOD.call_void(
                        uniffi_env,
                        completion,
                        [
                            {%- for (var, ffi_type) in return_type.ffi_values_rs("uniffi_return_lowered") %}
                            uniffi_jni::jvalue {
                                {{ ffi_type.jvalue_field() }}: {{ var }},
                            },
                            {%- endfor %}
                        ],
                    ).to_anyhow_result(uniffi_env, "{{ rust_result.async_complete_class() }}.complete")?;
                    {%- endif %}


        Ok(UNIFFI_RUST_FUTURE_COMPLETE)
    })
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ rust_result.async_cancel_fn() }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) {
    uniffi_jni::future_cancel::<{{ rust_result.return_type_rs() }}>(
        uniffi::Handle::from_raw_unchecked(handle as u64));
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ rust_result.async_free_fn() }}(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) {
    uniffi_jni::future_free::<{{ rust_result.return_type_rs() }}>(
        uniffi::Handle::from_raw_unchecked(handle as u64));
}
{%- endfor %}

{%- for callback_result in root.kotlin_async_callable_results() %}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ callback_result.async_complete_success_fn() }}(
    uniffi_env: *mut uniffi_jni::JNIEnv,
    _: *mut uniffi_jni::jclass,
    future_handle: i64,
    {%- if let Some(return_type) = callback_result.return_type %}
    {%- for ffi_type in return_type.ffi_types %}
    v{{ loop.index0 }}: {{ ffi_type.type_rs() }},
    {%- endfor %}
    {%- endif %}
) {
    uniffi::trace!("{{ callback_result.async_complete_success_fn() }}: {future_handle:x}");
    unsafe {
        let sender = uniffi::oneshot::Sender::<uniffi::Result<{{ callback_result.return_type_rs() }}>>::from_raw(
            ::std::ptr::with_exposed_provenance::<_>(future_handle as usize)
        );
        // Use `rust_call` rather than `rust_call_with_env`.  This way unexpected errors get sent
        // via the oneshot channel rather than turning into Kotlin exceptions.
        let return_result = uniffi_jni::rust_call(move || {
            {%- if let Some(return_type) = callback_result.return_type %}
            let uniffi_return = {{ return_type.lift_fn_rs() }}(
                uniffi_env,
                {%- for ffi_type in return_type.ffi_types %}
                v{{ loop.index0 }},
                {%- endfor %}
            )?;
            {%- else %}
            let uniffi_return = ();
            {%- endif %}
            {%- if callback_result.throws_type.is_some() %}
            uniffi::Result::Ok(::std::result::Result::Ok(uniffi_return))
            {%- else %}
            uniffi::Result::Ok(uniffi_return)
            {%- endif %}
        });
        sender.send(return_result.map_err(|msg| uniffi::deps::anyhow::anyhow!("{msg}")));
    }
}

{%- if let Some(throws_type) = callback_result.throws_type %}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ callback_result.async_complete_error_fn() }}(
    uniffi_env: *mut uniffi_jni::JNIEnv,
    _: *mut uniffi_jni::jclass,
    future_handle: i64,
    {%- for ffi_type in throws_type.ffi_types %}
    v{{ loop.index0 }}: {{ ffi_type.type_rs() }},
    {%- endfor %}
) {
    uniffi::trace!("{{ callback_result.async_complete_error_fn() }}: {future_handle:x}");
    unsafe {
        let sender = uniffi::oneshot::Sender::<uniffi::Result<{{ callback_result.return_type_rs() }}>>::from_raw(
            ::std::ptr::with_exposed_provenance::<_>(future_handle as usize)
        );
        // Use `rust_call` rather than `rust_call_with_env`.  This way unexpected errors get sent
        // via the oneshot channel rather than turning into Kotlin exceptions.
        let return_result = uniffi_jni::rust_call(|| {
            let err = {{ throws_type.lift_fn_rs() }}(
                uniffi_env,
                {%- for ffi_type in throws_type.ffi_types %}
                v{{ loop.index0 }},
                {%- endfor %}
            )?;
            uniffi::Result::Ok(::std::result::Result::Err(err))
        });
        sender.send(return_result.map_err(|msg| uniffi::deps::anyhow::anyhow!("{msg}")));
    }
}
{%- endif %}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_{{ callback_result.async_complete_unexpected_error_fn() }}(
    _: *mut uniffi_jni::JNIEnv,
    _: *mut uniffi_jni::jclass,
    future_handle: i64,
) {
    uniffi::trace!("{{ callback_result.async_complete_unexpected_error_fn() }}: {future_handle:x}");
    // Safety:
    // * We assume the Kotlin side sent us valid future handles
    let sender = unsafe {
        uniffi::oneshot::Sender::<uniffi::Result<{{ callback_result.return_type_rs() }}>>::from_raw(
            ::std::ptr::with_exposed_provenance::<_>(future_handle as usize)
        )
    };
    sender.send(uniffi::Result::Err(uniffi::deps::anyhow::anyhow!("Unexpected callback error")));
}
{%- endfor %}

struct UniffiKotlinCall<T: Send + 'static> {
    receiver: Option<uniffi::oneshot::Receiver<uniffi::Result<T>>>,
    key: i64,
    done: Option<uniffi::oneshot::Receiver<()>>,
}
impl<T: Send + 'static> Drop for UniffiKotlinCall<T> {
    fn drop(&mut self) {
        let Some(done) = self.done.take() else { return; };
        let receiver = self.receiver.take();
        static CANCEL: uniffi_jni::CachedStaticMethod = uniffi_jni::CachedStaticMethod::new(
            c"uniffi/UniffiKt", c"uniffiCancelCallback", c"(J)V",
        );
        unsafe {
            uniffi_jni::attach_current_thread(uniffi_get_global_jvm(), |env| {
                CANCEL.call_void(env, [uniffi_jni::jvalue { j: self.key }])
                    .warn_on_exception(env, "uniffiCancelCallback");
            });
        }
        if let Some(context) = uniffi::async_runtime::current_context() {
            context.defer_cleanup(async move {
                if let Some(receiver) = receiver { drop(receiver.await); }
                done.await;
            }).expect("callback cleanup owner closed");
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_kotlinCallbackReleased(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, handle: i64,
) {
    // Kotlin removes its Job and all captures before acknowledging release.
    let sender = uniffi::oneshot::Sender::<()>::from_raw(std::ptr::with_exposed_provenance(handle as usize));
    sender.send(());
}

async fn uniffi_kotlin_call<T: Send + 'static>(
    start: impl FnOnce(i64, i64) -> uniffi::Result<()> + Send,
) -> uniffi::Result<T> {
    let (sender, receiver) = uniffi::oneshot::channel::<uniffi::Result<T>>();
    let (released, done) = uniffi::oneshot::channel::<()>();
    let key = sender.into_raw().expose_provenance() as i64;
    let released = released.into_raw().expose_provenance() as i64;
    let mut owner = UniffiKotlinCall { key, receiver: Some(receiver), done: Some(done) };
    if let Err(error) = start(key, released) {
        // Dispatch either throws before registration, or transfers both senders
        // to the Kotlin Job. Generated dispatch does not throw after transfer.
        owner.done = None;
        unsafe {
            drop(uniffi::oneshot::Sender::<uniffi::Result<T>>::from_raw(std::ptr::with_exposed_provenance(key as usize)));
            drop(uniffi::oneshot::Sender::<()>::from_raw(std::ptr::with_exposed_provenance(released as usize)));
        }
        return Err(error);
    }
    let result = owner.receiver.as_mut().unwrap().await;
    owner.receiver = None;
    owner.done.as_mut().unwrap().await;
    owner.done = None;
    result
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_Scaffolding_inputCloseFinished(
    _: *mut uniffi_jni::JNIEnv, _: uniffi_jni::jclass, key: i64, success: bool,
) {
    let sender = uniffi::oneshot::Sender::<uniffi::Result<()>>::from_raw(std::ptr::with_exposed_provenance(key as usize));
    sender.send(if success { Ok(()) } else { Err(uniffi::deps::anyhow::anyhow!("input cleanup failed")) });
}
