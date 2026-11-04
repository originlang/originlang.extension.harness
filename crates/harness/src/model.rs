//! The model boundary.
//!
//! One trait, two implementations: a real OpenAI-compatible endpoint and a
//! scripted one. The scripted model is not a test-only shim — the CLI exposes it
//! as `--replay`, which makes the loop reproducible against recorded turns and
//! lets an end-to-end check run without a network or an API key.

use crate::error::{Error, Result};
use crate::message::{Message, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;

/// Tool description in the shape the model API expects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Name the model must emit.
    pub name: String,
    /// What the tool does, in one or two sentences.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

impl ToolSchema {
    /// Encodes one entry of the `tools` array.
    pub fn to_openai(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// Token accounting reported by the endpoint, when it reports at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub completion_tokens: u64,
}

/// Everything one completion needs.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    /// Conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call; empty disables tool calling for this turn.
    pub tools: Vec<ToolSchema>,
}

/// One assistant turn plus its usage.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// The assistant message.
    pub message: Message,
    /// Token usage, when reported.
    pub usage: Option<Usage>,
}

/// A completions endpoint.
///
/// Implementations take `&mut self` because a scripted model is stateful and a
/// real one may keep a connection pool later; the loop does not care which.
pub trait Model {
    /// Short label used in lifecycle events.
    fn label(&self) -> String;

    /// Requests the next assistant turn.
    fn complete(&mut self, request: &ModelRequest) -> Result<ModelResponse>;
}

/// Configuration for an OpenAI-compatible endpoint.
pub struct OpenAiConfig {
    /// API root without the trailing `/chat/completions`.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// Bearer token.
    pub api_key: String,
    /// Optional `max_tokens`.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: f32,
    /// Per-request wall-clock limit, enforced by curl.
    pub timeout: Duration,
}

// Hand-written so an accidental `{:?}` in a log cannot leak the token.
impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// OpenAI-compatible chat completions with tool calling.
pub struct OpenAiModel {
    config: OpenAiConfig,
}

impl OpenAiModel {
    /// Wraps a configuration.
    pub fn new(config: OpenAiConfig) -> Self {
        Self { config }
    }

    /// `<base_url>/chat/completions`, tolerating a trailing slash.
    pub fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn payload(&self, request: &ModelRequest) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": request.messages,
            "temperature": self.config.temperature,
        });
        if let Some(max_tokens) = self.config.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(ToolSchema::to_openai).collect());
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

impl Model for OpenAiModel {
    fn label(&self) -> String {
        self.config.model.clone()
    }

    fn complete(&mut self, request: &ModelRequest) -> Result<ModelResponse> {
        let body = serde_json::to_vec(&self.payload(request))?;
        let headers = [("Authorization", format!("Bearer {}", self.config.api_key))];
        let (status, text) =
            crate::http::post_json(&self.endpoint(), &headers, &body, self.config.timeout)?;

        if !(200..300).contains(&status) {
            return Err(Error::Model(format!(
                "{status} from {}: {}",
                self.endpoint(),
                snippet(&text, 600)
            )));
        }

        #[derive(Deserialize)]
        struct Completion {
            #[serde(default)]
            choices: Vec<Choice>,
            #[serde(default)]
            usage: Option<Usage>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }

        let completion: Completion = serde_json::from_str(&text)
            .map_err(|error| Error::Model(format!("unusable completion payload: {error}")))?;
        let choice = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Model("completion carried no choices".to_owned()))?;

        Ok(ModelResponse {
            message: choice.message,
            usage: completion.usage,
        })
    }
}

/// A model that replays recorded assistant turns, in order.
///
/// Each input line is one turn:
///
/// ```json
/// {"content":"done","tool_calls":[{"name":"note_save","arguments":{"key":"a"}}]}
/// ```
///
/// `arguments` may be an object or an already-encoded string; `id` is optional
/// and defaults to `call_<turn>_<index>`.
pub struct ScriptedModel {
    turns: std::collections::VecDeque<ModelResponse>,
    total: usize,
    served: usize,
}

#[derive(Deserialize)]
struct ScriptedTurn {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ScriptedCall {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

impl ScriptedModel {
    /// Parses newline-delimited turns; blank lines and `#` comments are skipped.
    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut turns = std::collections::VecDeque::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let turn: ScriptedTurn = serde_json::from_str(line)
                .map_err(|error| Error::Config(format!("replay line {}: {error}", index + 1)))?;
            let turn_number = turns.len() + 1;
            let tool_calls = turn
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(call_index, call)| {
                    let arguments = match call.arguments {
                        Some(Value::String(text)) => text,
                        Some(value) => value.to_string(),
                        None => "{}".to_owned(),
                    };
                    ToolCall::new(
                        call.id
                            .unwrap_or_else(|| format!("call_{turn_number}_{call_index}")),
                        call.name,
                        arguments,
                    )
                })
                .collect();
            turns.push_back(ModelResponse {
                message: Message::Assistant {
                    content: turn.content,
                    tool_calls,
                },
                usage: turn.usage,
            });
        }
        let total = turns.len();
        Ok(Self {
            turns,
            total,
            served: 0,
        })
    }

    /// Reads a replay file.
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            Error::Config(format!(
                "cannot read replay file {}: {error}",
                path.display()
            ))
        })?;
        Self::from_jsonl(&text)
    }

    /// Number of turns supplied.
    pub fn len(&self) -> usize {
        self.total
    }

    /// Whether the script is empty.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

impl Model for ScriptedModel {
    fn label(&self) -> String {
        format!("scripted({} turns)", self.total)
    }

    fn complete(&mut self, _request: &ModelRequest) -> Result<ModelResponse> {
        let response = self.turns.pop_front().ok_or_else(|| {
            Error::Model(format!(
                "replay exhausted after {} turns; the loop asked for turn {}",
                self.served,
                self.served + 1
            ))
        })?;
        self.served += 1;
        Ok(response)
    }
}

/// First `max` characters of `text`, with an explicit marker when cut.
fn snippet(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OpenAiConfig {
        OpenAiConfig {
            base_url: "https://example.test/v1/".to_owned(),
            model: "test-model".to_owned(),
            api_key: "sk-secret".to_owned(),
            max_tokens: Some(64),
            temperature: 0.2,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn endpoint_tolerates_trailing_slash() {
        let model = OpenAiModel::new(config());
        assert_eq!(model.endpoint(), "https://example.test/v1/chat/completions");
    }

    #[test]
    fn payload_declares_tools_only_when_present() {
        let model = OpenAiModel::new(config());
        let without = model.payload(&ModelRequest {
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
        });
        assert!(without.get("tools").is_none());
        assert!(without.get("tool_choice").is_none());

        let with = model.payload(&ModelRequest {
            messages: vec![Message::user("hi")],
            tools: vec![ToolSchema {
                name: "echo".to_owned(),
                description: "echo back".to_owned(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        });
        assert_eq!(with["tools"][0]["function"]["name"], json!("echo"));
        assert_eq!(with["tool_choice"], json!("auto"));
        assert_eq!(with["max_tokens"], json!(64));
    }

    #[test]
    fn debug_output_redacts_the_key() {
        let rendered = format!("{:?}", config());
        assert!(!rendered.contains("sk-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn scripted_model_encodes_arguments_as_strings() {
        let mut model = ScriptedModel::from_jsonl(
            r#"# a comment
{"tool_calls":[{"name":"note_save","arguments":{"key":"a"}}]}
{"content":"done"}
"#,
        )
        .expect("parse");
        assert_eq!(model.len(), 2);

        let first = model
            .complete(&ModelRequest {
                messages: Vec::new(),
                tools: Vec::new(),
            })
            .expect("turn");
        let calls = first.message.tool_calls();
        assert_eq!(calls[0].function.name, "note_save");
        assert_eq!(calls[0].function.arguments, r#"{"key":"a"}"#);
        assert_eq!(calls[0].id, "call_1_0");

        let second = model
            .complete(&ModelRequest {
                messages: Vec::new(),
                tools: Vec::new(),
            })
            .expect("turn");
        assert_eq!(second.message.content(), Some("done"));
    }

    #[test]
    fn scripted_model_reports_exhaustion() {
        let mut model = ScriptedModel::from_jsonl("").expect("parse");
        let error = model
            .complete(&ModelRequest {
                messages: Vec::new(),
                tools: Vec::new(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("exhausted"));
    }

    #[test]
    fn snippet_counts_characters_not_bytes() {
        assert_eq!(snippet("健康abc", 2), "健康…");
        assert_eq!(snippet("abc", 10), "abc");
    }
}
