/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::ffi::c_void;

use jni_sys::*;

/// Attach the current thread to the JVM and run a closure
///
/// This is used to implement callback interfaces.
///
/// The closure must not return any JNI objects,
/// since their lifetime ends before this function returns.
///
/// # Safety
///
/// jvm must point to a valid JavaVM
pub unsafe fn attach_current_thread<F, T>(jvm: *mut JavaVM, f: F) -> T
where
    F: FnOnce(*mut JNIEnv) -> T,
{
    let mut env: *mut JNIEnv = std::ptr::null_mut();
    // SAFETY: the caller provides a live VM. GetEnv only obtains this thread's
    // environment; a nested callback must not detach its Java caller.
    unsafe {
        let result = ((**jvm).v1_2.GetEnv)(
            jvm,
            std::ptr::from_mut(&mut env).cast::<*mut c_void>(),
            JNI_VERSION_1_2,
        );
        let attached_here = match result {
            JNI_OK => false,
            JNI_EDETACHED => {
                let result = ((**jvm).v1_2.AttachCurrentThread)(
                    jvm,
                    std::ptr::from_mut(&mut env).cast::<*mut c_void>(),
                    std::ptr::null_mut(),
                );
                assert_eq!(result, JNI_OK, "AttachCurrentThread failed");
                true
            }
            code => panic!("GetEnv failed: {code}"),
        };
        let mut guard = Attachment {
            jvm,
            env,
            attached_here,
            frame: false,
        };
        assert_eq!(
            ((**env).v1_2.PushLocalFrame)(env, 16),
            JNI_OK,
            "Out of memory: PushLocalFrame failed"
        );
        guard.frame = true;
        f(env)
    }
}

// Raw thread-local JNI pointers make this guard neither Send nor Sync. It is
// created before PushLocalFrame so attachment is also released on setup panic.
struct Attachment {
    jvm: *mut JavaVM,
    env: *mut JNIEnv,
    attached_here: bool,
    frame: bool,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // SAFETY: this guard stays on the acquiring thread. The closure and all
        // Java callbacks have returned (or unwound) before its frame is popped.
        unsafe {
            if self.frame {
                ((**self.env).v1_2.PopLocalFrame)(self.env, std::ptr::null_mut());
            }
            if self.attached_here {
                let result = ((**self.jvm).v1_2.DetachCurrentThread)(self.jvm);
                if result != JNI_OK {
                    eprintln!("DetachCurrentThread failed: {result}");
                }
            }
        }
    }
}
