//! LADEX on Android: the multicast lock, the network the phone is on, the notification that keeps it sharing, files
//! shared in from other apps, the folder received files are saved in, and handing saved files to other apps.
//!
//! Kotlin reports through a channel and is told what to do through JNI. Both keep working after Android has closed
//! the app's window while its sharing service runs, which Tauri's plugin calls do not.
#![cfg(target_os = "android")]

use std::sync::{Arc, OnceLock};

use jni::errors::Result as JniResult;
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::mobile::PluginInvokeError;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Runtime, WebviewWindow};

const KOTLIN_PACKAGE: &str = "io.github.gshreekar.ladex.android";
const PLUGIN_CLASS: &str = "LadexAndroidPlugin";
const BRIDGE_CLASS: &str = "io.github.gshreekar.ladex.android.DeviceBridge";

/// The plugin; once the app is built, `LadexAndroidExt::ladex_android` reaches it.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ladex-android")
        .setup(|app, api| {
            let handle = api.register_android_plugin(KOTLIN_PACKAGE, PLUGIN_CLASS)?;
            app.manage(LadexAndroid { handle, bridge: Arc::new(OnceLock::new()) });
            Ok(())
        })
        .build()
}

pub trait LadexAndroidExt<R: Runtime> {
    fn ladex_android(&self) -> &LadexAndroid<R>;
}

impl<R: Runtime, T: Manager<R>> LadexAndroidExt<R> for T {
    fn ladex_android(&self) -> &LadexAndroid<R> {
        self.state::<LadexAndroid<R>>().inner()
    }
}

pub struct LadexAndroid<R: Runtime> {
    handle: PluginHandle<R>,
    // Set shortly after startup, once the Kotlin side has been found through the window.
    bridge: Arc<OnceLock<Bridge>>,
}

#[derive(Serialize)]
struct Watch {
    events: Channel,
}

impl<R: Runtime> LadexAndroid<R> {
    /// Starts the reports of the phone's network and of the notification's Stop button; call once, while the window is open.
    pub fn watch<E: DeserializeOwned>(&self, on_event: impl Fn(E) + Send + Sync + 'static) -> Result<(), PluginInvokeError> {
        let events: Channel = Channel::new(move |body: InvokeResponseBody| {
            match body.deserialize::<E>() {
                Ok(event) => on_event(event),
                Err(error) => tracing::warn!("Android reported something LADEX does not understand: {error}"),
            }
            Ok(())
        });
        self.handle.run_mobile_plugin("watch", Watch { events })
    }

    /// Finds the Kotlin side through the window, so `apply` reaches it later even with no window.
    pub fn connect(&self, window: &WebviewWindow<R>) -> tauri::Result<()> {
        let bridge = self.bridge.clone();
        window.with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| match Bridge::find(env, activity) {
                Ok(found) => {
                    let _ = bridge.set(found);
                }
                Err(error) => tracing::error!("Could not reach LADEX's Android code: {error}"),
            });
        })
    }

    /// Tells the Kotlin side what the phone should do now; false if it could not be reached, so the caller tries again.
    pub fn apply(&self, wanted: &impl Serialize) -> bool {
        let Some(bridge) = self.bridge.get() else { return false };
        let json = match serde_json::to_string(wanted) {
            Ok(json) => json,
            Err(error) => {
                tracing::error!("Could not write LADEX's request to Android: {error}");
                return false;
            }
        };
        match bridge.apply(&json) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("Android did not take LADEX's request: {error}");
                false
            }
        }
    }

    /// Asks the Kotlin side something by the name of its `DeviceBridge` method; blocks, so call it off the async runtime.
    pub fn ask<T: DeserializeOwned>(&self, method: &str, question: &impl Serialize) -> Result<T, AskError> {
        let bridge = self.bridge.get().ok_or(AskError::NotReady)?;
        let json = serde_json::to_string(question).map_err(|error| AskError::Broken(error.to_string()))?;
        let answer = bridge.ask(method, &json).map_err(|error| AskError::Broken(error.to_string()))?;
        match serde_json::from_str(&answer).map_err(|error| AskError::Broken(format!("{error} in {answer}")))? {
            Answer::Refused { error } => Err(AskError::Refused(error)),
            Answer::Given(given) => Ok(given),
        }
    }
}

/// Why a question to the Android side has no answer to use.
#[derive(Debug)]
pub enum AskError {
    /// The Kotlin side has not been found yet.
    NotReady,
    /// Android answered with a refusal, in words fit for the person.
    Refused(String),
    /// The call itself failed, or its answer could not be read.
    Broken(String),
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady => f.write_str("LADEX's Android code is not ready yet"),
            Self::Refused(message) => f.write_str(message),
            Self::Broken(reason) => write!(f, "Android did not answer: {reason}"),
        }
    }
}

impl std::error::Error for AskError {}

// Kotlin answers every question with JSON, and a refusal as {"error": "..."}.
#[derive(Deserialize)]
#[serde(untagged)]
enum Answer<T> {
    Refused { error: String },
    Given(T),
}

struct Bridge {
    vm: JavaVM,
    class: GlobalRef,
}

impl Bridge {
    fn find(env: &mut JNIEnv, activity: &JObject) -> JniResult<Self> {
        let loaded = load_bridge_class(env, activity);
        clear_exception(env);
        let class = env.new_global_ref(loaded?)?;
        Ok(Self { vm: env.get_java_vm()?, class })
    }

    fn apply(&self, json: &str) -> JniResult<()> {
        let mut env = self.vm.attach_current_thread()?;
        let called = call_apply(&mut env, &self.class, json);
        clear_exception(&mut env);
        called
    }

    fn ask(&self, method: &str, json: &str) -> JniResult<String> {
        let mut env = self.vm.attach_current_thread()?;
        let called = call_ask(&mut env, &self.class, method, json);
        clear_exception(&mut env);
        called
    }
}

// The app's own classes are found through the activity's class loader; JNI's default loader only knows Android's.
fn load_bridge_class<'local>(env: &mut JNIEnv<'local>, activity: &JObject) -> JniResult<JObject<'local>> {
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string(BRIDGE_CLASS)?;
    env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()
}

fn call_apply(env: &mut JNIEnv, class: &GlobalRef, json: &str) -> JniResult<()> {
    let json = env.new_string(json)?;
    let class: &JClass = class.as_obj().into();
    env.call_static_method(class, "apply", "(Ljava/lang/String;)V", &[JValue::Object(&json)])?;
    Ok(())
}

fn call_ask(env: &mut JNIEnv, class: &GlobalRef, method: &str, json: &str) -> JniResult<String> {
    let json = env.new_string(json)?;
    let class: &JClass = class.as_obj().into();
    let answer = env.call_static_method(class, method, "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&json)])?.l()?;
    Ok(env.get_string(&JString::from(answer))?.into())
}

// A Java exception left pending would break the next JNI call made on this thread.
fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}
