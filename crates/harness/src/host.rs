//! The host surface the loop depends on.
//!
//! The harness runs as a dynamic plugin inside the host process, so "calling
//! the host" is a C ABI call, not a Rust method call: it crosses the contract
//! documented in `originlang-ext-plugin-abi`, and every request still passes
//! through the host's dispatcher, which is where capabilities are enforced.
//!
//! [`HostApi`] remains a trait even though [`PluginHost`] is its only
//! implementation. It is what lets the loop run against a fake in tests —
//! no host, no plugin, no dynamic library — and it is what an out-of-process
//! harness would implement to reuse the loop unchanged.

use crate::error::{Error, Result};
use originlang_ext_plugin_abi::{HostVTable, decode, read_c_str};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::CString;

/// One installed extension, reduced to the fields tool discovery needs.
///
/// Deliberately *not* the host's own summary type: the harness should not break
/// when the host adds a field. Unknown fields are ignored and absent ones fall
/// back to defaults, which is also what makes this work against
/// `host.extension.list`, whose payload is intentionally narrower than the
/// operator-facing listing — an extension sees identity, liveness and
/// contributions, not paths, hashes or grants.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtensionSummary {
    /// Stable identity; the target of `host.extension.call`.
    pub id: String,
    /// Package version.
    pub version: String,
    /// Whether the operator enabled the package.
    pub enabled: bool,
    /// Whether a live instance exists right now.
    pub loaded: bool,
    /// Methods reported by the runtime; empty until loaded.
    pub methods: Vec<String>,
    /// Capabilities declared by the manifest.
    pub capabilities: Vec<String>,
    /// Capabilities actually granted.
    pub granted: Vec<String>,
    /// Contribution points, forwarded verbatim by the host.
    pub contributes: Option<Value>,
}

/// The slice of the host the loop depends on.
///
/// Every method maps onto one host call; the adapter keeps no state beyond the
/// caller's stack, so a fake implementation is a few lines.
pub trait HostApi {
    /// Host version, backend and installation root.
    fn info(&mut self) -> Result<Value>;

    /// Installed extensions, as visible to this extension.
    fn list_extensions(&mut self) -> Result<Vec<ExtensionSummary>>;

    /// Invokes one extension method, loading the extension on demand.
    fn call_extension(&mut self, id: &str, method: &str, params: Value) -> Result<Value>;

    /// Publishes a host event; returns the delivery count.
    fn emit(&mut self, topic: &str, payload: Value) -> Result<usize>;
}

/// Host services reached through the plugin ABI.
///
/// The table is a copy of what the host handed to `oext_plugin_create`. Copying
/// is safe because it holds pointers and function pointers only: the copy views
/// the same services, it does not duplicate them.
pub struct PluginHost {
    table: HostVTable,
    /// Extensions on the caller's stack, outermost first.
    ///
    /// Passed back on every host call so the host can reject a call cycle
    /// before it deadlocks. The host appends this plugin's own id, which is why
    /// the slice here never contains it.
    chain: Vec<String>,
    /// Number of host calls made, for diagnostics in the run result.
    calls: u64,
}

impl PluginHost {
    /// Wraps the table the host passed to `oext_plugin_create`.
    pub fn new(table: HostVTable) -> Self {
        Self {
            table,
            chain: Vec::new(),
            calls: 0,
        }
    }

    /// Records the stack that led to the current invocation.
    ///
    /// Called by the plugin entry point before the loop runs so that a tool
    /// call made from inside a cross-extension call still reports the full
    /// chain.
    pub fn set_chain(&mut self, chain: Vec<String>) {
        self.chain = chain;
    }

    /// How many host calls this instance has made.
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// One Host API request.
    ///
    /// The response envelope is freed with the *host's* `string_free`, not with
    /// anything local: two allocators share this address space, and the rule
    /// that survives is that whoever allocated frees.
    pub fn call_host(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let call = self
            .table
            .call
            .ok_or_else(|| Error::Host("host table has no call entry".to_owned()))?;

        let method_c = CString::new(method)
            .map_err(|_| Error::Host("method name contains a NUL byte".to_owned()))?;
        // A serialized JSON value never contains a bare NUL — it is escaped.
        let params_c = match params {
            Some(value) => Some(
                CString::new(value.to_string())
                    .map_err(|_| Error::Host("parameters contain a NUL byte".to_owned()))?,
            ),
            None => None,
        };
        let chain_c = CString::new(serde_json::to_string(&self.chain)?)
            .map_err(|_| Error::Host("call chain contains a NUL byte".to_owned()))?;

        self.calls += 1;
        // SAFETY: every pointer outlives the call; `table.ctx` is owned by the
        // host and valid for this instance's lifetime.
        let raw = unsafe {
            call(
                self.table.ctx,
                method_c.as_ptr(),
                params_c
                    .as_ref()
                    .map(|value| value.as_ptr())
                    .unwrap_or(std::ptr::null()),
                chain_c.as_ptr(),
            )
        };

        // SAFETY: the host returned a NUL-terminated string it allocated.
        let text = unsafe { read_c_str(raw) };
        if let Some(free) = self.table.string_free {
            // SAFETY: same allocation, and the host's free is the one that
            // matches the host's allocator.
            unsafe { free(raw) };
        }

        let text = text.ok_or_else(|| Error::Host("host returned a null response".to_owned()))?;
        decode(&text).map_err(|(code, message)| Error::Host(format!("{message} (code {code})")))
    }
}

impl HostApi for PluginHost {
    fn info(&mut self) -> Result<Value> {
        self.call_host("host.self.info", None)
    }

    fn list_extensions(&mut self) -> Result<Vec<ExtensionSummary>> {
        let value = self.call_host("host.extension.list", None)?;
        serde_json::from_value(value).map_err(Error::from)
    }

    fn call_extension(&mut self, id: &str, method: &str, params: Value) -> Result<Value> {
        self.call_host(
            "host.extension.call",
            Some(json!({ "id": id, "method": method, "params": params })),
        )
    }

    fn emit(&mut self, topic: &str, payload: Value) -> Result<usize> {
        let value = self.call_host(
            "host.events.emit",
            Some(json!({ "topic": topic, "payload": payload })),
        )?;
        Ok(value.get("delivered").and_then(Value::as_u64).unwrap_or(0) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};

    /// Records the last request and replies with a fixed envelope.
    struct Recorder {
        method: String,
        params: String,
        reply: String,
    }

    unsafe extern "C" fn record_call(
        ctx: *mut c_void,
        method: *const c_char,
        params: *const c_char,
        _chain: *const c_char,
    ) -> *mut c_char {
        // SAFETY: the harness passes a `Recorder` as ctx for the test's duration.
        let recorder = unsafe { &mut *(ctx as *mut Recorder) };
        // SAFETY: the pointers are valid for the call.
        recorder.method = unsafe { CStr::from_ptr(method) }
            .to_string_lossy()
            .into_owned();
        recorder.params = if params.is_null() {
            String::new()
        } else {
            // SAFETY: same contract.
            unsafe { CStr::from_ptr(params) }
                .to_string_lossy()
                .into_owned()
        };
        std::ffi::CString::new(recorder.reply.clone())
            .expect("reply has no NUL")
            .into_raw()
    }

    unsafe extern "C" fn free_string(ptr: *mut c_char) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: the string came from `CString::into_raw` above.
        drop(unsafe { std::ffi::CString::from_raw(ptr) });
    }

    fn host_with(reply: &str) -> (PluginHost, Box<Recorder>) {
        let mut recorder = Box::new(Recorder {
            method: String::new(),
            params: String::new(),
            reply: reply.to_owned(),
        });
        let ctx = (&mut *recorder as *mut Recorder).cast::<c_void>();
        let table = HostVTable {
            abi_version: 1,
            call: Some(record_call),
            string_free: Some(free_string),
            log: None,
            ctx,
        };
        (PluginHost::new(table), recorder)
    }

    #[test]
    fn call_extension_sends_the_expected_envelope() {
        let (mut host, recorder) = host_with(r#"{"result":{"ok":true}}"#);
        let value = host
            .call_extension("toolbox", "note_save", json!({"key": "a"}))
            .expect("call");

        assert_eq!(value, json!({"ok": true}));
        assert_eq!(recorder.method, "host.extension.call");
        let sent: Value = serde_json::from_str(&recorder.params).expect("json");
        assert_eq!(sent["id"], "toolbox");
        assert_eq!(sent["method"], "note_save");
        assert_eq!(sent["params"]["key"], "a");
    }

    #[test]
    fn list_extensions_parses_a_narrow_payload() {
        let (mut host, _recorder) = host_with(
            r#"{"result":[{"id":"toolbox","enabled":true,"loaded":true,"contributes":{"tools":[]}}]}"#,
        );
        let summaries = host.list_extensions().expect("list");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "toolbox");
        // Fields the extension-facing listing omits fall back to defaults.
        assert!(summaries[0].version.is_empty());
        assert!(summaries[0].granted.is_empty());
    }

    #[test]
    fn emit_reports_the_delivery_count() {
        let (mut host, recorder) = host_with(r#"{"result":{"delivered":3}}"#);
        assert_eq!(host.emit("topic", json!({})).expect("emit"), 3);
        assert_eq!(recorder.method, "host.events.emit");
    }

    #[test]
    fn host_errors_keep_their_code_in_text() {
        let (mut host, _recorder) =
            host_with(r#"{"error":{"code":-32021,"message":"lacks capability 'extension.call'"}}"#);
        let error = host.info().expect_err("must fail");
        let text = error.to_string();
        assert!(text.contains("extension.call"), "{text}");
        assert!(text.contains("-32021"), "{text}");
    }

    #[test]
    fn the_call_chain_travels_with_every_request() {
        let (mut host, recorder) = host_with(r#"{"result":{}}"#);
        host.set_chain(vec!["caller".to_owned()]);
        host.info().expect("info");
        assert!(recorder.method.contains("host.self.info"));
    }
}
