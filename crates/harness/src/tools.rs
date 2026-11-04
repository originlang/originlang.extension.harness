//! Tool discovery and dispatch.
//!
//! A tool is an extension method plus a description the model can read. The
//! description comes from the extension itself, in `contributes.tools`, which
//! the host forwards verbatim — so installing a package is what makes a tool
//! available, and the harness keeps no second registry to drift out of sync.
//!
//! Dispatch always crosses the host. The harness never executes tool code
//! itself, so every call stays subject to the host's capability check, per-call
//! timeout, crash containment and log trail.

use crate::host::{ExtensionSummary, HostApi};
use crate::model::ToolSchema;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Largest observation handed back to the model, in bytes.
///
/// A tool that returns a 10 MiB document would evict the conversation from the
/// context window on the next turn. Truncating here keeps the loop alive and
/// tells the model what it lost; the full value is still reachable through the
/// host if a caller needs it.
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Which extension method a tool maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTarget {
    /// Extension id, as passed to the host.
    pub extension: String,
    /// Method name on that extension.
    pub method: String,
}

/// One callable tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Model-facing description.
    pub schema: ToolSchema,
    /// Host-side destination.
    pub target: ToolTarget,
}

/// Result of executing one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Whether the tool produced a value rather than an error.
    pub ok: bool,
    /// Observation text for the transcript.
    pub content: String,
}

impl ToolOutcome {
    /// Builds a failure observation the model can read and react to.
    pub fn error(kind: &str, message: impl Into<String>) -> Self {
        let content = json!({
            "error": {
                "kind": kind,
                "message": message.into(),
            }
        })
        .to_string();
        Self { ok: false, content }
    }
}

/// Tools keyed by name, in deterministic order.
#[derive(Debug, Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
    warnings: Vec<String>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry from installed extensions.
    ///
    /// Extensions are visited by id so a name collision resolves the same way on
    /// every run; the loser is reported through [`Self::warnings`] rather than
    /// dropped silently.
    pub fn discover(extensions: &[ExtensionSummary]) -> Self {
        let mut registry = Self::new();
        let mut ordered: Vec<&ExtensionSummary> = extensions.iter().collect();
        ordered.sort_by(|left, right| left.id.cmp(&right.id));

        for extension in ordered {
            if !extension.enabled {
                continue;
            }
            let mut warnings = Vec::new();
            let specs = tools_of(extension, &mut warnings);
            registry.warnings.append(&mut warnings);
            for spec in specs {
                registry.insert(spec, &extension.id);
            }
        }
        registry
    }

    /// Inserts one tool; the first declaration of a name wins.
    ///
    /// Returns `false` when the name was already taken.
    pub fn insert(&mut self, spec: ToolSpec, owner: &str) -> bool {
        let name = spec.schema.name.clone();
        if let Some(existing) = self.tools.get(&name) {
            let holder = existing.target.extension.clone();
            self.warnings.push(format!(
                "tool '{name}' from '{owner}' ignored: already provided by '{holder}'"
            ));
            return false;
        }
        self.tools.insert(name, spec);
        true
    }

    /// Looks a tool up by the name the model emitted.
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// Number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is available.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Tool names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Tools in registry order, for display.
    pub fn iter(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.values()
    }

    /// Schemas to advertise to the model.
    pub fn schema(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|spec| spec.schema.clone())
            .collect()
    }

    /// Non-fatal problems found during discovery.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

impl ToolSpec {
    /// Executes the tool through the host and shapes the observation.
    ///
    /// Argument errors do not raise: they become observations, because a model
    /// can usually fix its own JSON once it sees what went wrong. Host errors do
    /// the same, so one denied capability does not abort an otherwise healthy
    /// run.
    pub fn invoke(&self, host: &mut dyn HostApi, arguments: &str) -> ToolOutcome {
        let arguments = match parse_arguments(arguments) {
            Ok(value) => value,
            Err(message) => return ToolOutcome::error("arguments", message),
        };

        match host.call_extension(&self.target.extension, &self.target.method, arguments) {
            Ok(value) => {
                let encoded = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                ToolOutcome {
                    ok: true,
                    content: truncate(encoded),
                }
            }
            Err(error) => ToolOutcome::error("host", error.to_string()),
        }
    }
}

fn parse_arguments(arguments: &str) -> std::result::Result<Value, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err("arguments must be a JSON object".to_owned()),
        Err(error) => Err(format!("arguments are not valid JSON: {error}")),
    }
}

/// Reads `contributes.tools` from one extension.
fn tools_of(extension: &ExtensionSummary, warnings: &mut Vec<String>) -> Vec<ToolSpec> {
    let Some(contributes) = &extension.contributes else {
        return Vec::new();
    };
    let Some(entries) = contributes.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut specs = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            warnings.push(format!(
                "extension '{}': tools[{index}] has no name and was skipped",
                extension.id
            ));
            continue;
        };
        if !is_valid_tool_name(name) {
            warnings.push(format!(
                "extension '{}': tools[{index}] name {name:?} is not a valid tool name",
                extension.id
            ));
            continue;
        }

        let method = entry
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_owned();
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let parameters = entry
            .get("parameters")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

        specs.push(ToolSpec {
            schema: ToolSchema {
                name: name.to_owned(),
                description,
                parameters,
            },
            target: ToolTarget {
                extension: extension.id.clone(),
                method,
            },
        });
    }
    specs
}

/// Tool names are identifiers the model has to reproduce exactly.
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

fn truncate(content: String) -> String {
    if content.len() <= MAX_TOOL_RESULT_BYTES {
        return content;
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = content.len() - end;
    format!("{}\n… [truncated {dropped} bytes]", &content[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, Result};

    struct FakeHost {
        calls: Vec<(String, String, Value)>,
        reply: std::result::Result<Value, String>,
    }

    impl FakeHost {
        fn ok(value: Value) -> Self {
            Self {
                calls: Vec::new(),
                reply: Ok(value),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                calls: Vec::new(),
                reply: Err(message.to_owned()),
            }
        }
    }

    impl HostApi for FakeHost {
        fn info(&mut self) -> Result<Value> {
            Ok(Value::Null)
        }
        fn list_extensions(&mut self) -> Result<Vec<ExtensionSummary>> {
            Ok(Vec::new())
        }
        fn call_extension(&mut self, id: &str, method: &str, params: Value) -> Result<Value> {
            self.calls.push((id.to_owned(), method.to_owned(), params));
            match &self.reply {
                Ok(value) => Ok(value.clone()),
                Err(message) => Err(Error::Host(message.clone())),
            }
        }
        fn emit(&mut self, _topic: &str, _payload: Value) -> Result<usize> {
            Ok(0)
        }
    }

    fn extension(id: &str, tools: Value) -> ExtensionSummary {
        ExtensionSummary {
            id: id.to_owned(),
            enabled: true,
            contributes: Some(json!({ "tools": tools })),
            ..ExtensionSummary::default()
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            schema: ToolSchema {
                name: name.to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            },
            target: ToolTarget {
                extension: "toolbox".to_owned(),
                method: name.to_owned(),
            },
        }
    }

    #[test]
    fn discovers_tools_from_contributes() {
        let registry = ToolRegistry::discover(&[
            extension(
                "toolbox",
                json!([{
                    "name": "note_save",
                    "description": "Persist a note",
                    "parameters": {"type": "object", "properties": {"key": {"type": "string"}}}
                }]),
            ),
            extension("empty", json!([])),
        ]);
        assert_eq!(registry.names(), vec!["note_save"]);
        let spec = registry.get("note_save").expect("tool");
        assert_eq!(spec.target.extension, "toolbox");
        assert_eq!(spec.target.method, "note_save");
        assert_eq!(spec.schema.description, "Persist a note");
    }

    #[test]
    fn method_defaults_to_name_and_can_be_overridden() {
        let registry = ToolRegistry::discover(&[extension(
            "toolbox",
            json!([
                {"name": "a"},
                {"name": "b", "method": "do_b"}
            ]),
        )]);
        assert_eq!(registry.get("a").expect("a").target.method, "a");
        assert_eq!(registry.get("b").expect("b").target.method, "do_b");
        assert_eq!(
            registry.get("a").expect("a").schema.parameters["type"],
            json!("object"),
            "a tool without a schema must still be callable with no arguments"
        );
    }

    #[test]
    fn disabled_extensions_contribute_nothing() {
        let mut disabled = extension("toolbox", json!([{"name": "note_save"}]));
        disabled.enabled = false;
        assert!(ToolRegistry::discover(&[disabled]).is_empty());
    }

    #[test]
    fn invalid_names_are_reported_not_registered() {
        let registry = ToolRegistry::discover(&[extension(
            "toolbox",
            json!([{"name": "bad name!"}, {"name": ""}]),
        )]);
        assert!(registry.is_empty());
        assert_eq!(registry.warnings().len(), 2);
    }

    #[test]
    fn duplicate_names_resolve_by_extension_id() {
        let registry = ToolRegistry::discover(&[
            extension("zzz", json!([{"name": "echo", "description": "late"}])),
            extension("aaa", json!([{"name": "echo", "description": "early"}])),
        ]);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("echo").expect("echo").target.extension, "aaa");
        assert!(registry.warnings()[0].contains("already provided"));
    }

    #[test]
    fn malformed_entries_are_skipped_with_a_warning() {
        let registry =
            ToolRegistry::discover(&[extension("toolbox", json!([{"description": "x"}]))]);
        assert!(registry.is_empty());
        assert!(registry.warnings()[0].contains("no name"));
    }

    #[test]
    fn invoke_crosses_the_host_with_parsed_arguments() {
        let tool = spec("note_save");
        let mut host = FakeHost::ok(json!({"saved": "a"}));
        let outcome = tool.invoke(&mut host, r#"{"key":"a"}"#);

        assert!(outcome.ok);
        assert_eq!(outcome.content, r#"{"saved":"a"}"#);
        assert_eq!(host.calls.len(), 1);
        assert_eq!(host.calls[0].0, "toolbox");
        assert_eq!(host.calls[0].1, "note_save");
        assert_eq!(host.calls[0].2, json!({"key": "a"}));
    }

    #[test]
    fn empty_arguments_become_an_empty_object() {
        let mut host = FakeHost::ok(json!([]));
        spec("list").invoke(&mut host, "  ");
        assert_eq!(host.calls[0].2, json!({}));
    }

    #[test]
    fn invalid_arguments_become_an_observation_without_a_host_call() {
        let mut host = FakeHost::ok(json!(null));
        let outcome = spec("x").invoke(&mut host, "{not json");
        assert!(!outcome.ok);
        assert!(outcome.content.contains("not valid JSON"));
        assert!(
            host.calls.is_empty(),
            "dispatch must not happen on bad input"
        );
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let mut host = FakeHost::ok(json!(null));
        let outcome = spec("x").invoke(&mut host, "[1,2]");
        assert!(!outcome.ok);
        assert!(outcome.content.contains("must be a JSON object"));
    }

    #[test]
    fn host_denial_becomes_an_observation() {
        let mut host = FakeHost::failing("lacks capability 'storage.write'");
        let outcome = spec("note_save").invoke(&mut host, "{}");
        assert!(!outcome.ok);
        assert!(outcome.content.contains("storage.write"));
    }

    #[test]
    fn large_observations_are_truncated_on_a_char_boundary() {
        let payload = "健".repeat(MAX_TOOL_RESULT_BYTES);
        let truncated = truncate(payload);
        assert!(truncated.len() <= MAX_TOOL_RESULT_BYTES + 64);
        assert!(truncated.contains("truncated"));
        // The kept prefix is still valid UTF-8 by construction.
        assert!(truncated.starts_with("健健"));
    }

    #[test]
    fn small_observations_are_untouched() {
        assert_eq!(truncate("ok".to_owned()), "ok");
    }
}
