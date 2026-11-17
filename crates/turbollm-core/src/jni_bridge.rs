//! JNI surface used by the Kotlin app.
//!
//! Conventions:
//! * every object is an opaque `jlong` handle (a `Box<Engine>` / `Box<ChatSession>`);
//! * anything that can fail returns `null`/`0` and stores a JSON error in the
//!   thread-local, which Kotlin reads with [`nativeLastError`];
//! * generation streams through a callback (`onToken(String)`) so the UI can
//!   render tokens as they are produced.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jdouble, jfloat, jint, jlong, jstring};
use jni::JNIEnv;

use crate::api::{ChatMessage, GenParams};
use crate::engine::{Engine, KvType, LoadOptions};
use crate::session::ChatSession;

thread_local! {
    static LAST_ERROR: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn set_err(msg: impl Into<String>) {
    let msg = msg.into();
    tracing::error!("{msg}");
    LAST_ERROR.with(|e| *e.borrow_mut() = msg);
}

fn clear_err() {
    LAST_ERROR.with(|e| e.borrow_mut().clear());
}

fn engine_from(handle: jlong) -> Option<&'static Engine> {
    if handle == 0 {
        set_err("null engine handle");
        return None;
    }
    Some(unsafe { &*(handle as *const Engine) })
}

fn session_from(handle: jlong) -> Option<&'static mut ChatSession> {
    if handle == 0 {
        set_err("null session handle");
        return None;
    }
    Some(unsafe { &mut *(handle as *mut ChatSession) })
}

/// Wrap a fallible body, converting `Err` into `None` + `LastError`.
fn guard<T>(f: impl FnOnce() -> crate::Result<T>) -> Option<T> {
    clear_err();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            set_err(serde_json::json!({"code": e.code(), "message": e.to_string()}).to_string());
            None
        }
        Err(_) => {
            set_err(r#"{"code":"PANIC","message":"native panic"}"#);
            None
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeInit(_env: JNIEnv, _c: JClass) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let _ = crate::init_backend();
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeBackendInfo(
    env: JNIEnv,
    _c: JClass,
) -> jstring {
    let txt = guard(|| {
        let b = crate::init_backend()?;
        Ok(serde_json::json!({
            "gpu_offload": b.supports_gpu_offload(),
            "mmap": b.supports_mmap(),
            "mlock": b.supports_mlock(),
            "cpu_cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "threads_default": GenParams::default().n_threads,
        })
        .to_string())
    })
    .unwrap_or_default();
    env.new_string(txt)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Load a GGUF model. Returns an opaque handle or 0 on error.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeLoad(
    mut env: JNIEnv,
    _c: JClass,
    path: JString,
    n_ctx: jint,
    n_threads: jint,
    n_threads_batch: jint,
    n_gpu_layers: jint,
    kv_type: jint,
    flash_attn: jboolean,
) -> jlong {
    let path: String = match env.get_string(&path) {
        Ok(p) => p.into(),
        Err(e) => {
            set_err(format!("bad path: {e}"));
            return 0;
        }
    };
    guard(|| {
        let opts = LoadOptions {
            n_ctx: n_ctx.max(0) as u32,
            n_threads: n_threads.max(1),
            n_threads_batch: n_threads_batch.max(1),
            n_gpu_layers: n_gpu_layers.max(0) as u32,
            kv_type: match kv_type {
                0 => KvType::F16,
                2 => KvType::Q4_0,
                _ => KvType::Q8_0,
            },
            use_mmap: true,
            use_mlock: false,
            flash_attn: flash_attn != 0,
            n_batch: 512,
        };
        Ok(Box::into_raw(Box::new(Engine::load(path, opts)?)) as jlong)
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeFree(
    mut env: JNIEnv,
    _c: JClass,
    handle: jlong,
) {
    if handle != 0 {
        unsafe { drop(Box::from_raw(handle as *mut Engine)) };
    }
    let _ = &mut env;
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeModelInfo(
    env: JNIEnv,
    _c: JClass,
    handle: jlong,
) -> jstring {
    let txt = match engine_from(handle) {
        Some(e) => serde_json::to_string(e.info()).unwrap_or_default(),
        None => String::new(),
    };
    env.new_string(txt)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeCancel(
    _env: JNIEnv,
    _c: JClass,
    handle: jlong,
) {
    if let Some(e) = engine_from(handle) {
        e.cancel();
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeNewSession(
    mut env: JNIEnv,
    _c: JClass,
    engine_handle: jlong,
    system_prompt: JString,
) -> jlong {
    let system: String = env
        .get_string(&system_prompt)
        .map(Into::into)
        .unwrap_or_default();
    match engine_from(engine_handle) {
        Some(engine) => {
            let session = ChatSession::new(&system, GenParams::default());
            let _ = engine; // engine is kept alive by Kotlin
            Box::into_raw(Box::new(session)) as jlong
        }
        None => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeFreeSession(
    _env: JNIEnv,
    _c: JClass,
    handle: jlong,
) {
    if handle != 0 {
        unsafe { drop(Box::from_raw(handle as *mut ChatSession)) };
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeResetSession(
    _env: JNIEnv,
    _c: JClass,
    session: jlong,
    engine: jlong,
) {
    if let (Some(s), Some(e)) = (session_from(session), engine_from(engine)) {
        s.reset(e);
    }
}

/// JSON with generation parameters, so adding a knob does not change the ABI.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeGenerate(
    mut env: JNIEnv,
    _c: JClass,
    session_handle: jlong,
    engine_handle: jlong,
    messages_json: JString,
    params_json: JString,
    callback: JObject,
) -> jstring {
    let messages: String = match env.get_string(&messages_json) {
        Ok(s) => s.into(),
        Err(e) => {
            set_err(format!("bad messages json: {e}"));
            return std::ptr::null_mut();
        }
    };
    let params: String = match env.get_string(&params_json) {
        Ok(s) => s.into(),
        Err(e) => {
            set_err(format!("bad params json: {e}"));
            return std::ptr::null_mut();
        }
    };

    let session = match session_from(session_handle) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let engine = match engine_from(engine_handle) {
        Some(e) => e,
        None => return std::ptr::null_mut(),
    };

    let msgs: Vec<ChatMessage> = match serde_json::from_str(&messages) {
        Ok(m) => m,
        Err(e) => {
            set_err(format!("messages parse: {e}"));
            return std::ptr::null_mut();
        }
    };
    let p: GenParams = match serde_json::from_str::<GenParams>(&params) {
        Ok(p) => p,
        Err(e) => {
            set_err(format!("params parse: {e}"));
            return std::ptr::null_mut();
        }
    };
    session.set_params(p);

    // Detach the callback object so it can be called from this thread.
    let cb = match env.new_global_ref(&callback) {
        Ok(c) => c,
        Err(e) => {
            set_err(format!("callback: {e}"));
            return std::ptr::null_mut();
        }
    };
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            set_err(format!("vm: {e}"));
            return std::ptr::null_mut();
        }
    };
    let mut cb_env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            set_err(format!("attach: {e}"));
            return std::ptr::null_mut();
        }
    };

    // One local reference per token would overflow the (default 512 entries)
    // local-ref table within the first answer and abort the process. Each token
    // therefore runs inside its own push/pop local frame.
    let on_token = |piece: &str| {
        if cb_env.push_local_frame(8).is_err() {
            return;
        }
        if let Ok(js) = cb_env.new_string(piece) {
            let _ = cb_env.call_method(&cb, "onToken", "(Ljava/lang/String;)V", &[(&js).into()]);
        }
        // SAFETY: every local ref created above is dead at this point.
        unsafe {
            let _ = cb_env.pop_local_frame(&JObject::null());
        }
    };

    let result = guard(|| {
        let (text, stats) = session.generate(engine, &msgs, on_token)?;
        Ok(serde_json::json!({ "text": text, "stats": stats }).to_string())
    });

    // Drop the callback/env refs before we hand the result back: the Java string
    // is created outside the frame so it survives the pop.
    drop(cb_env);
    match result {
        Some(json) => env
            .new_string(json)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeLastError(
    env: JNIEnv,
    _c: JClass,
) -> jstring {
    let msg = LAST_ERROR.with(|e| e.borrow().clone());
    env.new_string(msg)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// "Did the KV cache already contain this prompt?" stats for the UI badge.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeCachedHits(
    _env: JNIEnv,
    _c: JClass,
    session_handle: jlong,
) -> jdouble {
    session_from(session_handle)
        .map(|s| s.cached_hits as f64)
        .unwrap_or(0.0)
}

/// Quick self-test helper used by the CI smoke test: generates from a plain
/// prompt without the JNI callback, returns JSON with text + stats.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeGenerateBench(
    mut env: JNIEnv,
    _c: JClass,
    engine_handle: jlong,
    prompt: JString,
    max_tokens: jint,
) -> jstring {
    let prompt: String = match env.get_string(&prompt) {
        Ok(p) => p.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    let engine = match engine_from(engine_handle) {
        Some(e) => e,
        None => return std::ptr::null_mut(),
    };
    let out = guard(|| {
        let mut session = ChatSession::new(
            "",
            GenParams {
                max_tokens: max_tokens.max(1),
                ..Default::default()
            },
        );
        let (text, stats) = session.generate(engine, &[ChatMessage::user(prompt)], |_| {})?;
        Ok(serde_json::json!({"text": text, "stats": stats}).to_string())
    });
    match out {
        Some(j) => env
            .new_string(j)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[allow(dead_code)]
fn _assert_thread_safe() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<Engine>>();
    let _: jfloat = 0.0;
}

// Keep the unused-import linters quiet about the ABI types we use in signatures.
const _: fn() = || {
    let _ = std::mem::size_of::<jint>();
    let _ = std::mem::size_of::<jlong>();
    let _ = std::mem::size_of::<jdouble>();
};

// ===========================================================================
// v0.2 — agent, tools, MCP and plugins
// ===========================================================================
//
// Everything below follows the same conventions as the chat API above: opaque
// `jlong` handles, JSON in/out, errors in `nativeLastError`, and a Java callback
// object for streaming events (tokens, tool calls, tool results).

use crate::agent::{AgentConfig, AgentEvent};
use crate::plugins::{self, InstalledPlugin, PluginManifest};
use crate::tools::{self, ToolCall, ToolSpec, ToolStyle};

/// The agent callback interface implemented in Java (`AgentSink`).
const FN_ON_EVENT: &str = "(Ljava/lang/String;)V";
/// Tool executor: `(String name, String argsJson) -> String resultJson`.
const FN_EXECUTE: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

/// Notify Java about one agent event.
fn emit(env: &mut JNIEnv, sink: &JObject, event: &AgentEvent) {
    // Same local-ref discipline as the token callback above.
    if env.push_local_frame(8).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(event) {
        if let Ok(js) = env.new_string(json) {
            let _ = env.call_method(sink, "onEvent", FN_ON_EVENT, &[(&js).into()]);
        }
    }
    // SAFETY: the event string created above is no longer referenced.
    unsafe {
        let _ = env.pop_local_frame(&JObject::null());
    }
}

/// Run a tool that the app owns (HTTP tool, Android API or an MCP server).
fn call_java_executor(
    env: &mut JNIEnv,
    executor: &JObject,
    call: &ToolCall,
) -> std::result::Result<String, String> {
    let name = env
        .new_string(&call.name)
        .map_err(|e| format!("jni: {e}"))?;
    let args = env
        .new_string(call.arguments.to_string())
        .map_err(|e| format!("jni: {e}"))?;
    let res = env
        .call_method(
            executor,
            "execute",
            FN_EXECUTE,
            &[(&name).into(), (&args).into()],
        )
        .map_err(|e| format!("executor: {e}"))?;
    let obj = res.l().map_err(|e| format!("executor: {e}"))?;
    // `obj` is a local ref owned by the caller's frame; it is released when the
    // agent loop's frame pops.
    if obj.is_null() {
        return Err("инструмент не вернул результат".into());
    }
    let js = JString::from(obj);
    let text: String = env
        .get_string(&js)
        .map_err(|e| format!("executor result: {e}"))?
        .into();
    // The app answers with {"ok":bool,"output":string}; plain text is accepted too.
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) if v.get("ok").is_some() => {
            let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
            let out = v
                .get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            if ok {
                Ok(out)
            } else {
                Err(out)
            }
        }
        _ => Ok(text),
    }
}

/// Built-in tool names, so the UI can show a checklist without hardcoding.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeBuiltinTools(
    env: JNIEnv,
    _c: JClass,
) -> jstring {
    let specs = crate::builtin_tools::builtin_specs();
    let json = serde_json::to_string(&specs).unwrap_or_default();
    env.new_string(json)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Agent loop: model ↔ tools until a final answer.
///
/// * `tools_json` — JSON array of `ToolSpec` (built-in, plugin and MCP tools).
/// * `config_json` — JSON `AgentConfig` (max_steps, force_json, style, gen).
/// * `sink` — object with `onEvent(String json)`.
/// * `executor` — object with `execute(String name, String argsJson) -> String`.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeRunAgent(
    mut env: JNIEnv,
    _c: JClass,
    session_handle: jlong,
    engine_handle: jlong,
    messages_json: JString,
    tools_json: JString,
    config_json: JString,
    sink: JObject,
    executor: JObject,
) -> jstring {
    let messages: String = match env.get_string(&messages_json) {
        Ok(v) => v.into(),
        Err(e) => {
            set_err(format!("messages: {e}"));
            return std::ptr::null_mut();
        }
    };
    let tools_raw: String = match env.get_string(&tools_json) {
        Ok(v) => v.into(),
        Err(e) => {
            set_err(format!("tools: {e}"));
            return std::ptr::null_mut();
        }
    };
    let config_raw: String = match env.get_string(&config_json) {
        Ok(v) => v.into(),
        Err(e) => {
            set_err(format!("config: {e}"));
            return std::ptr::null_mut();
        }
    };

    let session = match session_from(session_handle) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let engine = match engine_from(engine_handle) {
        Some(e) => e,
        None => return std::ptr::null_mut(),
    };

    let history: Vec<crate::api::ChatMessage> = match serde_json::from_str(&messages) {
        Ok(v) => v,
        Err(e) => {
            set_err(format!("messages parse: {e}"));
            return std::ptr::null_mut();
        }
    };
    let tool_specs: Vec<ToolSpec> = match serde_json::from_str(&tools_raw) {
        Ok(v) => v,
        Err(e) => {
            set_err(format!("tools parse: {e}"));
            return std::ptr::null_mut();
        }
    };
    let mut cfg: AgentConfig = match serde_json::from_str(&config_raw) {
        Ok(v) => v,
        Err(e) => {
            set_err(format!("config parse: {e}"));
            return std::ptr::null_mut();
        }
    };
    if cfg.max_steps == 0 {
        cfg.max_steps = 1;
    }

    let sink = match env.new_global_ref(&sink) {
        Ok(s) => s,
        Err(e) => {
            set_err(format!("sink: {e}"));
            return std::ptr::null_mut();
        }
    };
    let executor = match env.new_global_ref(&executor) {
        Ok(s) => s,
        Err(e) => {
            set_err(format!("executor: {e}"));
            return std::ptr::null_mut();
        }
    };
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            set_err(format!("vm: {e}"));
            return std::ptr::null_mut();
        }
    };
    let cb_env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            set_err(format!("attach: {e}"));
            return std::ptr::null_mut();
        }
    };

    // `agent::run` needs two closures that both touch the JNI env, so share it
    // through a `RefCell` (single-threaded here: the agent runs on this thread).
    let env_cell = std::cell::RefCell::new(cb_env);
    let mut executor_fn = |call: &ToolCall| -> std::result::Result<String, String> {
        call_java_executor(&mut env_cell.borrow_mut(), &executor, call)
    };
    let emit_fn = |ev: AgentEvent| emit(&mut env_cell.borrow_mut(), &sink, &ev);

    let out = guard(|| {
        let (text, stats) = crate::agent::run(
            session,
            engine,
            &history,
            &tool_specs,
            &cfg,
            &mut executor_fn,
            emit_fn,
        )?;
        Ok(serde_json::json!({"text": text, "stats": stats}).to_string())
    });

    match out {
        Some(json) => env
            .new_string(json)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// Validate a plugin manifest (`turbollm.plugin.json`) before installing.
/// Returns `{"ok":true,"manifest":{…},"tools":[…]}`
/// or `{"ok":false,"errors":["…"]}`.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativePluginCheck(
    mut env: JNIEnv,
    _c: JClass,
    manifest_json: JString,
) -> jstring {
    let raw: String = match env.get_string(&manifest_json) {
        Ok(v) => v.into(),
        Err(e) => {
            set_err(format!("manifest: {e}"));
            return std::ptr::null_mut();
        }
    };
    let out = match serde_json::from_str::<PluginManifest>(&raw) {
        Ok(m) => {
            let errs = plugins::validate(&m);
            if errs.is_empty() {
                let tools: Vec<ToolSpec> = m.tools.iter().map(|t| t.to_spec(&m.name)).collect();
                serde_json::json!({
                    "ok": true,
                    "manifest": m,
                    "tools": tools,
                    "prompt": m.instructions.clone(),
                })
            } else {
                serde_json::json!({"ok": false, "errors": errs})
            }
        }
        Err(e) => serde_json::json!({
            "ok": false,
            "errors": [format!("не удалось разобрать манифест: {e}")]
        }),
    };
    env.new_string(out.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Parse `owner/repo[@ref][/subdir]` and return the URLs to try, so the app
/// does not have to duplicate that logic.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativePluginSources(
    mut env: JNIEnv,
    _c: JClass,
    source: JString,
) -> jstring {
    let raw: String = match env.get_string(&source) {
        Ok(v) => v.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    let out = match plugins::GitHubRef::parse(&raw) {
        Ok(r) => serde_json::json!({
            "ok": true,
            "owner": r.owner,
            "repo": r.repo,
            "ref": r.git_ref,
            "subdir": r.subdir,
            "manifest_urls": r.manifest_urls(),
            "archive_url": r.archive_url(),
        }),
        Err(e) => serde_json::json!({"ok": false, "error": e}),
    };
    env.new_string(out.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Strip YAML front matter from a SKILL.md (skills are Claude-style markdown).
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeStripSkill(
    mut env: JNIEnv,
    _c: JClass,
    markdown: JString,
) -> jstring {
    let raw: String = env
        .get_string(&markdown)
        .map(Into::into)
        .unwrap_or_default();
    env.new_string(plugins::strip_front_matter(&raw))
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Build the MCP `initialize` request line (JSON-RPC + newline) for a server.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpInitialize(
    mut env: JNIEnv,
    _c: JClass,
    client_version: JString,
) -> jstring {
    let ver: String = env
        .get_string(&client_version)
        .map(Into::into)
        .unwrap_or_else(|_| "0.2.0".to_string());
    let line = crate::mcp::initialize_request(1, "TurboLLM", &ver).to_line();
    env.new_string(line)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// `notifications/initialized` line that must follow a successful initialize.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpInitialized(
    env: JNIEnv,
    _c: JClass,
) -> jstring {
    env.new_string(crate::mcp::initialized_notification())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Build any MCP request: `tools/list` (pass `""` as tool) or `tools/call`.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpRequest(
    mut env: JNIEnv,
    _c: JClass,
    id: jlong,
    method: JString,
    params_json: JString,
) -> jstring {
    let method: String = env.get_string(&method).map(Into::into).unwrap_or_default();
    let params_raw: String = env
        .get_string(&params_json)
        .map(Into::into)
        .unwrap_or_else(|_| "null".to_string());
    let params: Option<serde_json::Value> = match serde_json::from_str(&params_raw) {
        Ok(serde_json::Value::Null) | Err(_) => None,
        Ok(v) => Some(v),
    };
    let req = if method == "initialize" {
        crate::mcp::initialize_request(id as u64, "TurboLLM", "0.2.0")
    } else {
        crate::mcp::RpcRequest::new(id as u64, method.clone(), params)
    };
    env.new_string(req.to_line())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Parse one MCP response line → `{"ok":true,"result":…}` / `{"ok":false,…}`,
/// or `{"notification":true}` for server notifications (which the app ignores).
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpParse(
    mut env: JNIEnv,
    _c: JClass,
    line: JString,
) -> jstring {
    let raw: String = env.get_string(&line).map(Into::into).unwrap_or_default();
    let out = if raw.trim().is_empty() {
        serde_json::json!({"notification": true})
    } else {
        match crate::mcp::RpcResponse::parse(&raw) {
            None => serde_json::json!({"notification": true}),
            Some(r) => match r.error {
                Some(e) => serde_json::json!({
                    "ok": false, "code": e.code, "error": e.message
                }),
                None => serde_json::json!({
                    "ok": true, "result": r.result.unwrap_or(serde_json::Value::Null)
                }),
            },
        }
    };
    env.new_string(out.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// `tools/list` result → app tool specs (namespaced as `server__tool`).
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpTools(
    mut env: JNIEnv,
    _c: JClass,
    server: JString,
    result_json: JString,
) -> jstring {
    let server: String = env.get_string(&server).map(Into::into).unwrap_or_default();
    let raw: String = env
        .get_string(&result_json)
        .map(Into::into)
        .unwrap_or_default();
    let result: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let specs = crate::mcp::parse_tools_list(&server, &result);
    env.new_string(serde_json::to_string(&specs).unwrap_or_default())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// `tools/call` result → `{"ok":bool,"output":"…"}`.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeMcpResult(
    mut env: JNIEnv,
    _c: JClass,
    result_json: JString,
) -> jstring {
    let raw: String = env
        .get_string(&result_json)
        .map(Into::into)
        .unwrap_or_default();
    let result: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let (text, is_err) = crate::mcp::parse_tool_result(&result);
    let out = serde_json::json!({"ok": !is_err, "output": text});
    env.new_string(out.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Tool-call syntax the model was trained on (`qwen`, `llama3`, `json`).
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeDetectToolStyle(
    env: JNIEnv,
    _c: JClass,
    engine_handle: jlong,
) -> jstring {
    let style = match engine_from(engine_handle) {
        Some(e) => {
            let tpl = e.chat_template();
            tools::detect_style(&e.info().description, tpl.as_deref(), ToolStyle::Auto)
        }
        None => ToolStyle::Qwen,
    };
    let name = match style {
        ToolStyle::Llama3 => "llama3",
        ToolStyle::Json => "json",
        _ => "qwen",
    };
    env.new_string(name)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Append `<tool_response>` + continue, i.e. what the agent does between steps.
/// Exposed for the app so it can replay a session deterministically.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativeToolResponsePrompt(
    mut env: JNIEnv,
    _c: JClass,
    output: JString,
    limit: jint,
) -> jstring {
    let out: String = env.get_string(&output).map(Into::into).unwrap_or_default();
    let limit = limit.max(256) as usize;
    let body: String = if out.chars().count() > limit {
        out.chars().take(limit).collect()
    } else {
        out
    };
    let text = format!(
        "<tool_response>\n{body}\n</tool_response>\n\nContinue. Use another tool only if it is really needed, otherwise give the final answer to the user."
    );
    env.new_string(text)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Installed-plugin helper: prompt addendum (instructions + skills) as JSON.
#[no_mangle]
pub extern "system" fn Java_ai_turbollm_app_NativeBridge_nativePluginPrompt(
    mut env: JNIEnv,
    _c: JClass,
    plugin_json: JString,
) -> jstring {
    let raw: String = env
        .get_string(&plugin_json)
        .map(Into::into)
        .unwrap_or_default();
    let out = match serde_json::from_str::<InstalledPlugin>(&raw) {
        Ok(p) => serde_json::json!({
            "ok": true,
            "prompt": p.prompt_addendum(),
            "tools": p.tools(),
        }),
        Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
    };
    env.new_string(out.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
