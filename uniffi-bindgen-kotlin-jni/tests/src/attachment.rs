//! Real JVM checks for Java-owned and temporarily attached native threads.
use std::ffi::c_void;
use uniffi_bindgen_kotlin_jni_runtime as jni;

unsafe fn status(vm: *mut jni::JavaVM) -> i32 {
    let mut env: *mut c_void = std::ptr::null_mut();
    unsafe { ((**vm).v1_2.GetEnv)(vm, &mut env, jni::JNI_VERSION_1_2) }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_checkAttachment(
    env: *mut jni::JNIEnv,
    _: jni::jclass,
) -> bool {
    // SAFETY: invoked by this JVM with its current thread's JNIEnv. The worker
    // is joined before returning, so the VM remains alive for every access.
    unsafe {
        let mut vm: *mut jni::JavaVM = std::ptr::null_mut();
        if ((**env).v1_2.GetJavaVM)(env, &mut vm) != jni::JNI_OK {
            return false;
        }
        if status(vm) != jni::JNI_OK {
            return false;
        }
        let nested = jni::attach_current_thread(vm, |outer| {
            jni::attach_current_thread(vm, |inner| outer == inner)
        });
        if !nested || status(vm) != jni::JNI_OK {
            return false;
        }
        let address = vm.expose_provenance();
        std::thread::spawn(move || {
            let vm: *mut jni::JavaVM = std::ptr::with_exposed_provenance_mut(address);
            if status(vm) != jni::JNI_EDETACHED {
                return false;
            }
            let nested = jni::attach_current_thread(vm, |outer| {
                let inner_ok = jni::attach_current_thread(vm, |inner| outer == inner);
                inner_ok && status(vm) == jni::JNI_OK
            });
            nested && status(vm) == jni::JNI_EDETACHED
        })
        .join()
        .unwrap_or(false)
            && status(vm) == jni::JNI_OK
    }
}
