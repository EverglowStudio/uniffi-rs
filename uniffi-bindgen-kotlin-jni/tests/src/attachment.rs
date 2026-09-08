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

// Receiver tests use real Run futures without changing the existing attachment
// assertions above. Control calls join each detached producer before returning.
use jni::uniffi;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::task::{Poll, Waker};

#[derive(Default)]
struct ReceiverState {
    body: AtomicBool,
    cleanup: AtomicBool,
    dropped: AtomicBool,
    cleaned: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
fn receiver_states() -> &'static Mutex<std::collections::HashMap<u64, Arc<ReceiverState>>> {
    static STATES: OnceLock<Mutex<std::collections::HashMap<u64, Arc<ReceiverState>>>> =
        OnceLock::new();
    STATES.get_or_init(Default::default)
}
struct ReceiverDrop(Arc<ReceiverState>);
impl Drop for ReceiverDrop {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
        let state = self.0.clone();
        uniffi::async_runtime::current_context()
            .unwrap()
            .defer_cleanup(std::future::poll_fn(move |cx| {
                *state.waker.lock().unwrap() = Some(cx.waker().clone());
                if state.cleanup.load(Ordering::SeqCst) {
                    state.cleaned.store(true, Ordering::SeqCst);
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }))
            .unwrap();
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_uniffi_JniRuntimeProbe_receiverNew(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    mode: i32,
) -> i64 {
    let state = Arc::new(ReceiverState::default());
    state.body.store(mode != 0, Ordering::SeqCst);
    state.cleanup.store(mode != 0, Ordering::SeqCst);
    let owned = state.clone();
    let handle = jni::future_new(async move {
        let _drop = ReceiverDrop(owned.clone());
        let mut early = mode == 2;
        std::future::poll_fn(move |cx| {
            *owned.waker.lock().unwrap() = Some(cx.waker().clone());
            if std::mem::take(&mut early) {
                for _ in 0..100 {
                    cx.waker().wake_by_ref();
                }
                return Poll::Pending;
            }
            if owned.body.load(Ordering::SeqCst) {
                Poll::Ready(Ok(47u32))
            } else {
                Poll::Pending
            }
        })
        .await
    });
    receiver_states()
        .lock()
        .unwrap()
        .insert(handle.as_raw(), state);
    handle.as_raw() as i64
}

#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_receiverPoll(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
    id: i64,
) -> bool {
    // SAFETY: the owned Kotlin test serializes polls through the product await
    // helper and keeps the typed handle live until complete/free.
    unsafe {
        jni::future_poll::<u32>(
            uniffi::Handle::from_raw_unchecked(handle as u64),
            jni::future_notification_enqueue,
            id as u64,
        )
    }
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_receiverCancel(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
) {
    unsafe {
        jni::future_cancel::<u32>(uniffi::Handle::from_raw_unchecked(handle as u64));
    }
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_receiverComplete(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
) -> i32 {
    // -1 denotes actual Run cancellation, not a fabricated notification result.
    unsafe { jni::future_complete::<u32>(uniffi::Handle::from_raw_unchecked(handle as u64)) }
        .map(|v| v as i32)
        .unwrap_or_else(|error| {
            assert!(jni::is_cancelled(&error));
            -1
        })
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_receiverFree(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
) {
    let state = receiver_states()
        .lock()
        .unwrap()
        .remove(&(handle as u64))
        .unwrap();
    assert!(state.cleaned.load(Ordering::SeqCst));
    unsafe {
        jni::future_free::<u32>(uniffi::Handle::from_raw_unchecked(handle as u64));
    }
}
#[unsafe(no_mangle)]
extern "system" fn Java_uniffi_JniRuntimeProbe_receiverDropped(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
) -> bool {
    receiver_states()
        .lock()
        .unwrap()
        .get(&(handle as u64))
        .unwrap()
        .dropped
        .load(Ordering::SeqCst)
}
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_receiverWake(
    env: *mut jni::JNIEnv,
    _: jni::jclass,
    handle: i64,
    phase: i32,
) -> bool {
    // SAFETY: the Java caller keeps this JVM and future live until this joined
    // thread exits. GetEnv is used solely to assert no attachment by native wake.
    unsafe {
        let mut vm = std::ptr::null_mut();
        assert_eq!(((**env).v1_2.GetJavaVM)(env, &mut vm), jni::JNI_OK);
        let address = vm.expose_provenance();
        let state = receiver_states()
            .lock()
            .unwrap()
            .get(&(handle as u64))
            .unwrap()
            .clone();
        std::thread::spawn(move || {
            let vm = std::ptr::with_exposed_provenance_mut::<jni::JavaVM>(address);
            assert_eq!(status(vm), jni::JNI_EDETACHED);
            if phase == 1 {
                state.body.store(true, Ordering::SeqCst);
            }
            if phase == 2 {
                state.cleanup.store(true, Ordering::SeqCst);
            }
            let waker = state.waker.lock().unwrap().clone().unwrap();
            for _ in 0..100 {
                waker.wake_by_ref();
                assert_eq!(status(vm), jni::JNI_EDETACHED);
            }
            true
        })
        .join()
        .unwrap()
    }
}

// Independent opt6 gate: feed a real native conversion failure through the
// generated u32 completion, rather than substituting its Kotlin terminal lambda.
#[unsafe(no_mangle)]
extern "system" fn Java_uniffi_JniRuntimeProbe_conversionFailureNew(
    _: *mut jni::JNIEnv,
    _: jni::jclass,
) -> i64 {
    jni::future_new::<u32>(async {
        Err(uniffi::LiftArgsError {
            arg_name: "opt6 conversion gate",
            error: uniffi::deps::anyhow::anyhow!("opt6 native conversion failure"),
        })
    })
    .as_raw() as i64
}

// Exercise the generated String completion's existing throwing lift through
// the same JNI callback/error helpers used by generated native completion.
#[unsafe(no_mangle)]
unsafe extern "system" fn Java_uniffi_JniRuntimeProbe_completionLiftFailure(
    env: *mut jni::JNIEnv,
    _: jni::jclass,
    completion: jni::jobject,
) {
    use jni::JniResultExt;
    static COMPLETE: jni::CachedMethod = jni::CachedMethod::new(
        c"uniffi/CompleteRustFuture11",
        c"complete",
        c"(Ljava/lang/String;)V",
    );
    // SAFETY: the owned test passes an actual generated CompleteRustFuture11;
    // null is a valid JNI String reference and intentionally fails liftString.
    // This probe owns no future handle; it tests only the real lift/error bridge.
    unsafe {
        jni::rust_call_with_env(env, |env| {
            COMPLETE
                .call_void(
                    env,
                    completion,
                    [jni::jvalue {
                        l: std::ptr::null_mut(),
                    }],
                )
                .to_anyhow_result(env, "CompleteRustFuture11.complete")
        });
    }
}
