//! Conversation messages, shaped for the OpenAI chat-completions schema.
//!
//! The transcript is the harness's only durable state, so the types here are
//! exactly what goes on the wire: no private fields, no separate DTO. A
//! transcript can therefore be replayed into any client that speaks the same
//! protocol.

use serde::{Deserialize, Serialize};

/// One message in the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// Instruction that frames the whole run.
    System {
        /// Prompt text.
        content: String,
    },
    /// The operator's goal.
    User {
        /// Goal text.
        content: String,
    },
    /// A model turn: prose, tool calls, or both.
    Assistant {
        /// Prose answer; `null` when the turn is only tool calls.
        content: Option<String>,
        /// Requested tool invocations, empty for a final answer.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// The observation produced by executing one tool call.
    Tool {
        /// Id of the call this observation answers.
        tool_call_id: String,
        /// Serialized observation; always a string on the wire.
        content: String,
    },
}

impl Message {
    /// Builds a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    /// Builds a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    /// Builds a tool observation.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }

    /// Prose carried by this message, if any.
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::System { content } | Self::User { content } => Some(content),
            Self::Assistant { content, .. } => content.as_deref(),
            Self::Tool { content, .. } => Some(content),
        }
    }

    /// Tool calls carried by this message; empty for every other role.
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Self::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }

    /// Character count of the prose, used for lifecycle events only.
    pub fn content_chars(&self) -> usize {
        self.content()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
    }
}

/// One requested tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id, echoed back in the matching [`Message::Tool`].
    pub id: String,
    /// Always `function` today; kept explicit because the protocol requires it.
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    /// Name plus JSON-encoded arguments.
    pub function: FunctionCall,
}

impl ToolCall {
    /// Builds a function call with JSON-encoded arguments.
    ///
    /// `arguments` is taken as `impl Into<String>` because callers naturally
    /// have either a literal or a serialized value, and forcing one of them to
    /// allocate just to satisfy the signature helps nobody.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: function_kind(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

/// Function name and raw argument payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Tool name, as advertised in `contributes.tools`.
    pub name: String,
    /// JSON object encoded as a string, exactly as the model produced it.
    pub arguments: String,
}

fn function_kind() -> String {
    "function".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_tool_call_round_trips() {
        let text = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "note_save", "arguments": "{\"key\":\"a\"}"}
            }]
        })
        .to_string();
        let message: Message = serde_json::from_str(&text).expect("parse");
        let calls = message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "note_save");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(message.content(), None);

        let encoded = serde_json::to_value(&message).expect("encode");
        assert_eq!(
            encoded["tool_calls"][0]["function"]["name"],
            json!("note_save")
        );
    }

    #[test]
    fn final_answer_omits_empty_tool_calls() {
        let message = Message::Assistant {
            content: Some("done".to_owned()),
            tool_calls: Vec::new(),
        };
        let encoded = serde_json::to_value(&message).expect("encode");
        assert!(encoded.get("tool_calls").is_none());
        assert_eq!(encoded["content"], json!("done"));
    }

    #[test]
    fn tool_observation_carries_call_id() {
        let encoded = serde_json::to_value(Message::tool("call_9", "{}")).expect("encode");
        assert_eq!(encoded["role"], json!("tool"));
        assert_eq!(encoded["tool_call_id"], json!("call_9"));
    }
}
