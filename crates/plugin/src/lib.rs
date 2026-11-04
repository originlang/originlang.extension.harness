//! The harness exported as a loadable host plugin.
//!
//! This crate is the whole plugin: eight C exports, one opaque handle, and a
//! thin translation from JSON parameters to [`Runner`]. All the loop logic
//! lives in `originlang-ext-harness`, which knows nothing about the C ABI.
//!
//! # Why every export is wrapped
//!
//! A panic escaping an `extern "C"` function aborts the process, and here that
//! process is the host: one `unwrap()` in this crate would take down every
//! other extension. Every entry point therefore runs inside
//! [`originlang_ext_plugin_abi::guard`], which turns a panic into an error
//! envelope the host can report.
//!
//! # Credentials
//!
//! `run` accepts `apiKey` (inline, for development) or `apiKeyEnv` (the name of
//! an environment variable, preferred). The environment route keeps the secret
//! out of call parameters, which are logged and visible in the transcript; the
//! plugin reads it from its own process, which is the host's process.

// 导出的八个符号只由 C ABI 调用（宿主通过 libloading 取符号），Rust 侧没有任何
// 调用点。指针有效性属于 ABI 契约，Rust 类型系统表达不了；每个入口在解引用前
// 都先判空，panic 由 guard 兜住。因此不把导出函数标成 unsafe —— 那只会在宿主
// 侧多一层无意义的 unsafe 块，并不会让契约更安全。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use originlang_ext_harness::{
    HostApi, Limits, OpenAiConfig, OpenAiModel, PluginHost, Runner, Silent, ToolRegistry,
};
use originlang_ext_plugin_abi::{
    ABI_VERSION, Handle, HostVTable, SYM_ABI_VERSION, SYM_CALL, SYM_CREATE, SYM_EMIT,
    SYM_IS_RUNNING, SYM_REGISTER, SYM_SHUTDOWN, SYM_STRING_FREE, free_c_string, guard, read_c_str,
    read_json,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

/// Manifest identity this plugin reports.
const PLUGIN_ID: &str = "harness";
/// Version, kept in step with the crate.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Capabilities the plugin asks for, mirrored by its manifest.
///
/// Declared here as well because the host reports what the code says it needs
/// and what the operator granted as two separate facts; a mismatch is a
/// packaging bug worth seeing.
const CAPABILITIES: [&str; 2] = ["extension.call", "log.write"];

/// One live plugin instance.
struct Instance {
    host: PluginHost,
    running: bool,
}

impl Instance {
    fn new(table: HostVTable) -> Self {
        Self {
            host: PluginHost::new(table),
            running: true,
        }
    }

    /// Runs one agent turn sequence and returns its result.
    fn run(&mut self, params: Option<Value>) -> Result<Value, (i32, String)> {
        let params = params.ok_or((-32602, "run requires parameters".to_owned()))?;
        let request: RunParams = serde_json::from_value(params)
            .map_err(|error| (-32602, format!("invalid parameters: {error}")))?;

        // Discovery first: an empty tool table is a valid configuration (the
        // model can still answer from its own knowledge), so it is not an error.
        let extensions = self
            .host
            .list_extensions()
            .map_err(|error| (-32005, error.to_string()))?;
        let tools = ToolRegistry::discover(&extensions);

        let mut model = OpenAiModel::new(request.model.into_config()?);
        let mut observer = Silent;

        let mut runner = Runner::new(&mut model, &mut self.host, &tools, &mut observer)
            .with_limits(request.limits.into_limits());
        if let Some(prompt) = request.system_prompt {
            runner = runner.with_system_prompt(prompt);
        }

        let result = runner.run(&request.goal);
        serde_json::to_value(&result)
            .map_err(|error| (-32603, format!("cannot encode the result: {error}")))
    }

    /// Reports what the loop would see as its tool surface.
    ///
    /// Exists so that "why did the model not use my tool" can be answered
    /// without running a model: the answer is usually a package that is
    /// disabled, or a `contributes.tools` entry the registry rejected.
    fn tools(&mut self) -> Result<Value, (i32, String)> {
        let extensions = self
            .host
            .list_extensions()
            .map_err(|error| (-32005, error.to_string()))?;
        let registry = ToolRegistry::discover(&extensions);
        Ok(json!({
            "extensions": extensions.len(),
            "tools": registry.names(),
            "warnings": registry.warnings(),
        }))
    }
}

/// Parameters of `run`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunParams {
    /// What the agent should accomplish.
    goal: String,
    /// Model endpoint and credentials.
    model: ModelParams,
    /// Budget overrides.
    #[serde(default)]
    limits: LimitParams,
    /// Replaces the built-in system prompt.
    #[serde(default)]
    system_prompt: Option<String>,
}

/// Model endpoint and credentials.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelParams {
    /// API root, without `/chat/completions`.
    base_url: String,
    /// Model identifier.
    model: String,
    /// Inline bearer token; convenient and visible in logs.
    #[serde(default)]
    api_key: Option<String>,
    /// Name of the environment variable holding the token.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Optional `max_tokens`.
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default = "default_temperature")]
    temperature: f32,
    /// Per-request wall-clock limit.
    #[serde(default = "default_request_timeout_ms")]
    request_timeout_ms: u64,
}

impl ModelParams {
    fn into_config(self) -> Result<OpenAiConfig, (i32, String)> {
        // Inline first, then the named variable, then the conventional one:
        // a caller that names nothing still works in a normally configured
        // environment.
        let api_key = match (self.api_key, self.api_key_env) {
            (Some(key), _) if !key.is_empty() => key,
            (_, Some(name)) => read_env(&name)?,
            _ => read_env("OPENAI_API_KEY")?,
        };

        Ok(OpenAiConfig {
            base_url: self.base_url,
            model: self.model,
            api_key,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            timeout: Duration::from_millis(self.request_timeout_ms),
        })
    }
}

/// Budget overrides; absent fields keep [`Limits::default`].
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LimitParams {
    max_model_calls: Option<u32>,
    max_tool_calls: Option<u32>,
    /// `0` disables the deadline.
    timeout_ms: Option<u64>,
}

impl LimitParams {
    fn into_limits(self) -> Limits {
        let mut limits = Limits::default();
        if let Some(value) = self.max_model_calls {
            limits.max_model_calls = value;
        }
        if let Some(value) = self.max_tool_calls {
            limits.max_tool_calls = value;
        }
        if let Some(value) = self.timeout_ms {
            limits.timeout = if value == 0 {
                None
            } else {
                Some(Duration::from_millis(value))
            };
        }
        limits
    }
}

fn default_temperature() -> f32 {
    0.2
}

fn default_request_timeout_ms() -> u64 {
    120_000
}

fn read_env(name: &str) -> Result<String, (i32, String)> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err((
            -32022,
            format!("no API key: set '{name}' or pass apiKey in the request"),
        )),
    }
}

/// Borrows the instance behind a handle.
///
/// # Safety
///
/// `handle` must be a live handle returned by [`oext_plugin_create`] and not yet
/// shut down.
unsafe fn instance_mut<'a>(handle: Handle) -> Result<&'a mut Instance, (i32, String)> {
    if handle.is_null() {
        return Err((-32005, "null plugin handle".to_owned()));
    }
    // SAFETY: guaranteed by the caller; the host serializes calls.
    Ok(unsafe { &mut *(handle as *mut Instance) })
}

/// Reads the caller's stack out of a JSON array.
fn parse_chain(ptr: *const c_char) -> Vec<String> {
    // SAFETY: the host passes a null or NUL-terminated string for this call.
    match unsafe { read_json(ptr) } {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reports the ABI revision. The host checks this before binding anything else.
///
/// # Safety
///
/// Exported as a C symbol; called by the host through `libloading`.
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_abi_version() -> u32 {
    ABI_VERSION
}

/// Builds an instance from the host's table.
///
/// Returns null on a null table or a panic; the host treats null as "refused to
/// load" and reports it rather than aborting.
///
/// # Safety
///
/// `host` must point to a valid [`HostVTable`] that outlives the instance.
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_create(host: *const HostVTable) -> Handle {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: null was rejected above and the host keeps the table alive.
        let table = unsafe { *host };
        Box::into_raw(Box::new(Instance::new(table))).cast::<std::ffi::c_void>()
    })) {
        Ok(handle) => handle,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Reports identity, methods and declared capabilities.
///
/// # Safety
///
/// `handle` must be a live handle from [`oext_plugin_create`].
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_register(handle: Handle) -> *mut c_char {
    guard(|| {
        // SAFETY: forwarded to `instance_mut`, which documents the contract.
        let _instance = unsafe { instance_mut(handle) }?;
        Ok(json!({
            "id": PLUGIN_ID,
            "version": PLUGIN_VERSION,
            "methods": ["run", "tools", "ping"],
            "capabilities": CAPABILITIES,
        }))
    })
}

/// Invokes one method.
///
/// # Safety
///
/// `handle` must be live; `method`, `params_json` and `chain_json` must be null
/// or NUL-terminated strings valid for the call.
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_call(
    handle: Handle,
    method: *const c_char,
    params_json: *const c_char,
    chain_json: *const c_char,
) -> *mut c_char {
    guard(|| {
        // SAFETY: forwarded to `instance_mut`.
        let instance = unsafe { instance_mut(handle) }?;
        // SAFETY: the host passes a NUL-terminated method name.
        let method = unsafe { read_c_str(method) }
            .ok_or((-32600, "method is null or not UTF-8".to_owned()))?;
        // SAFETY: the host passes null or a NUL-terminated JSON string.
        let params = unsafe { read_json(params_json) };

        instance.host.set_chain(parse_chain(chain_json));

        match method.as_str() {
            "ping" => Ok(json!({ "pong": true, "version": PLUGIN_VERSION })),
            "tools" => instance.tools(),
            "run" => instance.run(params),
            other => Err((-32601, format!("unknown method '{other}'"))),
        }
    })
}

/// Delivers one event.
///
/// The harness is a driver, not a subscriber: it produces work rather than
/// reacting to it, so events are accepted and ignored. Returning success rather
/// than an error keeps the host from retrying a delivery that has no consumer.
///
/// # Safety
///
/// `handle` must be a live handle from [`oext_plugin_create`].
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_emit(
    handle: Handle,
    _event_json: *const c_char,
    _chain_json: *const c_char,
) -> *mut c_char {
    guard(|| {
        // SAFETY: forwarded to `instance_mut`.
        let _instance = unsafe { instance_mut(handle) }?;
        Ok(Value::Null)
    })
}

/// Reports whether the instance can still serve calls.
///
/// # Safety
///
/// `handle` must be null or a live handle from [`oext_plugin_create`].
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_is_running(handle: Handle) -> i32 {
    if handle.is_null() {
        return 0;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded to `instance_mut`.
        let instance = unsafe { instance_mut(handle) };
        matches!(instance, Ok(instance) if instance.running)
    })) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Runs the cleanup hook and releases the instance.
///
/// # Safety
///
/// `handle` must come from [`oext_plugin_create`] and must not be used again
/// afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_shutdown(handle: Handle) -> *mut c_char {
    guard(|| {
        if handle.is_null() {
            return Ok(Value::Null);
        }
        // SAFETY: the handle came from `Box::into_raw` in `create` and is
        // reclaimed exactly once.
        let mut instance = unsafe { Box::from_raw(handle as *mut Instance) };
        instance.running = false;
        drop(instance);
        Ok(Value::Null)
    })
}

/// Frees a string this plugin returned.
///
/// # Safety
///
/// `ptr` must come from `plugin_abi::encode*` in this library and must not have
/// been freed already.
#[unsafe(no_mangle)]
pub extern "C" fn oext_plugin_string_free(ptr: *mut c_char) {
    // SAFETY: forwarded to the allocator that produced it.
    unsafe { free_c_string(ptr) };
}

/// Keeps the exported symbol names discoverable to a reader grepping this file.
const _: &[&str] = &[
    SYM_ABI_VERSION,
    SYM_CREATE,
    SYM_REGISTER,
    SYM_CALL,
    SYM_EMIT,
    SYM_IS_RUNNING,
    SYM_SHUTDOWN,
    SYM_STRING_FREE,
];
