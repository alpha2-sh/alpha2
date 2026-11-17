//! The JNI exports uniffi cannot express, called from the Kotlin `Alpha2Android` object.
//!
//! The app calls `Alpha2Android.init(context)` once; it caches the Java VM and the application
//! context for `ndk_context`, which audio output and codec listing read.

use std::ffi::c_void;
use std::sync::OnceLock;

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use jni::sys::{JNI_VERSION_1_6, jint, jlong};

static VM: OnceLock<jni::JavaVM> = OnceLock::new();

/// Caches the Java VM when the library loads.
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: jni::JavaVM, _reserved: *mut c_void) -> jint {
    let _ = VM.set(vm);
    JNI_VERSION_1_6
}

/// `sh.alpha2.Alpha2Android.init(Context)`: keeps the application context for `ndk_context`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_alpha2_Alpha2Android_init(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    static INITIALIZED: OnceLock<()> = OnceLock::new();
    let Some(vm) = VM.get() else {
        let _ = env.throw_new("java/lang/IllegalStateException", "JNI_OnLoad did not run");
        return;
    };
    let application = env
        .call_method(
            &context,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .and_then(jni::objects::JValueGen::l);
    let application = match application {
        Ok(application) if !application.is_null() => application,
        _ => context,
    };
    let global = match env.new_global_ref(application) {
        Ok(global) => global,
        Err(error) => {
            let _ = env.throw_new("java/lang/IllegalStateException", error.to_string());
            return;
        }
    };
    INITIALIZED.get_or_init(|| {
        // SAFETY: the VM lives for the process, and the global reference is leaked so the context
        // pointer stays valid for it too.
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_java_vm_pointer().cast(),
                global.as_obj().as_raw().cast(),
            );
        }
        std::mem::forget(global);
    });
}

/// The cached Java VM, once `JNI_OnLoad` ran.
#[must_use]
pub fn java_vm() -> Option<&'static jni::JavaVM> {
    VM.get()
}

/// `sh.alpha2.Alpha2Android.windowFromSurface(Surface)`: the `ANativeWindow` of a `Surface`, as
/// the handle `Player.attachWindow` takes over; 0 for a surface without one.
#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_alpha2_Alpha2Android_windowFromSurface(
    env: JNIEnv,
    _class: JClass,
    surface: JObject,
) -> jlong {
    // SAFETY: the environment and surface come from the JVM for this call; the returned window
    // holds its own reference.
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast())
    };
    window as jlong
}
