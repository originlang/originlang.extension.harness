//! The loop.
//!
//! One goal in, one transcript out. Each iteration asks the model for a turn,
//! records it, executes the requested tools through the host, and feeds the
//! observations back. The loop ends on a final answer, a spent budget, or a
//! model failure — never on a tool failure, because a failed tool is information
//! the model can act on.
//!
//! Termination is structural rather than cooperative: every iteration spends
//! from a [`Budget`] that is checked before the spend, so even a model that
//! loops forever stops at a known counter. There is no cancellation token,
//! because the loop performs no blocking wait the caller could interrupt other
//! than the model request itself, which is bounded by the model timeout.

use crate::budget::{Budget, Exhausted, Limits};
use crate::host::HostApi;
use crate::message::Message;
use crate::model::{Model, ModelRequest};
use crate::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Host event published once per run, so extensions can observe the harness.
///
/// Best-effort: a host with no subscriber, or a host that rejects the topic,
/// must not fail a run that otherwise succeeded.
pub const FINISHED_TOPIC: &str = "harness.run.finished";

/// Default framing prompt.
///
/// Kept short and factual: the tool list carries the capability description, so
/// the prompt only needs to state the contract (one tool call per step, stop by
/// answering without a call) and the honesty rule about observations.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are an agent running inside the OriginLang extension host. Extensions provide the tools; \
each tool call is executed by the host under its capability policy, and you receive the result \
as JSON.

Rules:
- Call tools when they provide facts you do not have. Do not invent observations.
- If a tool returns an error object, read it: fix the arguments and retry, or choose another tool.
- When the goal is met, answer in plain text and call no tool. That ends the run.
- Answer in the language of the goal.";

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a final answer.
    Completed,
    /// The model produced neither prose nor tool calls.
    EmptyAnswer,
    /// `max_model_calls` was reached.
    ModelBudget,
    /// `max_tool_calls` was reached.
    ToolBudget,
    /// The wall-clock budget was spent.
    Deadline,
    /// The model endpoint failed.
    ModelError,
}

impl StopReason {
    /// Whether the goal was answered.
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Completed)
    }

    fn from_exhausted(exhausted: Exhausted) -> Self {
        match exhausted {
            Exhausted::ModelCalls => Self::ModelBudget,
            Exhausted::ToolCalls => Self::ToolBudget,
            Exhausted::Deadline => Self::Deadline,
        }
    }
}

/// Coarse outcome, for callers that only branch on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The goal was answered.
    Completed,
    /// The run stopped early; see `stop_reason` and `error`.
    Incomplete,
}

impl Status {
    fn of(reason: StopReason) -> Self {
        if reason.succeeded() {
            Self::Completed
        } else {
            Self::Incomplete
        }
    }
}

/// Counters for one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Assistant turns requested.
    pub model_calls: u32,
    /// Tool invocations executed.
    pub tool_calls: u32,
    /// Wall-clock duration.
    pub elapsed_ms: u64,
    /// Prompt tokens reported by the endpoint, summed.
    pub prompt_tokens: u64,
    /// Completion tokens reported by the endpoint, summed.
    pub completion_tokens: u64,
}

/// Everything a caller needs after a run.
///
/// Serializable on purpose: this is what the calling entry point prints on
/// stdout and writes to `result.json`, so an operator can diff two runs without
/// re-reading logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// Identifier, also used as the transcript directory name.
    pub run_id: String,
    /// Coarse outcome.
    pub status: Status,
    /// Why the loop stopped.
    pub stop_reason: StopReason,
    /// The goal as given.
    pub goal: String,
    /// Final answer, when the model produced one.
    pub answer: Option<String>,
    /// Failure text for `model_error`; `None` otherwise.
    pub error: Option<String>,
    /// Full conversation, oldest first.
    pub messages: Vec<Message>,
    /// Counters.
    pub stats: Stats,
}

impl RunResult {
    /// Process exit code: `0` answered, `1` stopped early.
    pub fn exit_code(&self) -> i32 {
        if self.status == Status::Completed {
            0
        } else {
            1
        }
    }
}

/// One lifecycle event.
///
/// Events are the operator's view of a run in progress: the CLI prints them as
/// JSONL on stderr and persists them next to the transcript. They carry sizes and
/// names, never message bodies, so a log line cannot leak a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The run started.
    RunStarted {
        /// Model label.
        model: String,
        /// Discovered tool count.
        tools: usize,
        /// Goal length in characters.
        goal_chars: usize,
    },
    /// A model turn was requested.
    ModelCall {
        /// 1-based turn index.
        index: u32,
    },
    /// The model answered.
    AssistantTurn {
        /// Prose length in characters.
        content_chars: usize,
        /// Number of requested tool calls.
        tool_calls: usize,
    },
    /// A tool call is about to be dispatched.
    ToolCall {
        /// Tool name.
        name: String,
        /// Target extension.
        extension: String,
        /// Target method.
        method: String,
    },
    /// A tool call returned.
    ToolResult {
        /// Tool name.
        name: String,
        /// Whether it produced a value.
        ok: bool,
        /// Observation size in bytes.
        bytes: usize,
    },
    /// The run ended.
    RunStopped {
        /// Why.
        reason: StopReason,
        /// Coarse outcome.
        status: Status,
        /// Counters.
        stats: Stats,
    },
}

/// Receives lifecycle events.
///
/// A trait rather than a callback so a caller can both print and collect: the
/// CLI needs the JSONL stream, and the transcript needs the same events as an
/// array.
pub trait Observer {
    /// Handles one event.
    fn observe(&mut self, event: &Event);
}

/// An observer that discards everything.
pub struct Silent;

impl Observer for Silent {
    fn observe(&mut self, _event: &Event) {}
}

/// Collects events in memory.
#[derive(Debug, Default)]
pub struct Recorder {
    events: Vec<Event>,
}

impl Recorder {
    /// Recorded events, in order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

impl Observer for Recorder {
    fn observe(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
}

/// Drives one run.
pub struct Runner<'a> {
    model: &'a mut dyn Model,
    host: &'a mut dyn HostApi,
    tools: &'a ToolRegistry,
    limits: Limits,
    system_prompt: String,
    observer: &'a mut dyn Observer,
}

impl<'a> Runner<'a> {
    /// Wires the loop; budgets come from [`Limits::default`] unless overridden
    /// with [`Runner::with_limits`].
    pub fn new(
        model: &'a mut dyn Model,
        host: &'a mut dyn HostApi,
        tools: &'a ToolRegistry,
        observer: &'a mut dyn Observer,
    ) -> Self {
        Self {
            model,
            host,
            tools,
            limits: Limits::default(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
            observer,
        }
    }

    /// Replaces the budget.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Runs until the goal is answered or a budget is spent.
    pub fn run(&mut self, goal: &str) -> RunResult {
        let run_id = new_run_id();
        let mut budget = Budget::new(self.limits.clone());
        let mut messages = vec![
            Message::system(self.system_prompt.clone()),
            Message::user(goal),
        ];
        let mut stats = Stats::default();

        self.observer.observe(&Event::RunStarted {
            model: self.model.label(),
            tools: self.tools.len(),
            goal_chars: goal.chars().count(),
        });

        let mut answer = None;
        let mut error = None;

        let stop_reason = loop {
            let index = match budget.begin_model_call() {
                Ok(index) => index,
                Err(exhausted) => break StopReason::from_exhausted(exhausted),
            };
            self.observer.observe(&Event::ModelCall { index });

            let request = ModelRequest {
                messages: messages.clone(),
                tools: self.tools.schema(),
            };
            let response = match self.model.complete(&request) {
                Ok(response) => response,
                Err(failure) => {
                    error = Some(failure.to_string());
                    break StopReason::ModelError;
                }
            };
            if let Some(usage) = response.usage {
                stats.prompt_tokens += usage.prompt_tokens;
                stats.completion_tokens += usage.completion_tokens;
            }
            stats.model_calls = budget.model_calls();

            let message = response.message;
            let calls = message.tool_calls().to_vec();
            self.observer.observe(&Event::AssistantTurn {
                content_chars: message.content_chars(),
                tool_calls: calls.len(),
            });
            messages.push(message.clone());

            if calls.is_empty() {
                let text = message.content().unwrap_or_default().trim().to_owned();
                if text.is_empty() {
                    break StopReason::EmptyAnswer;
                }
                answer = Some(text);
                break StopReason::Completed;
            }

            if let Err(exhausted) = budget.charge_tools(calls.len()) {
                // The requested calls are reported as unexecuted rather than
                // silently dropped: the caller can see why the run stopped.
                break StopReason::from_exhausted(exhausted);
            }

            for call in &calls {
                let name = call.function.name.clone();
                let outcome = match self.tools.get(&name) {
                    Some(spec) => {
                        self.observer.observe(&Event::ToolCall {
                            name: name.clone(),
                            extension: spec.target.extension.clone(),
                            method: spec.target.method.clone(),
                        });
                        spec.invoke(self.host, &call.function.arguments)
                    }
                    None => crate::tools::ToolOutcome {
                        ok: false,
                        content: json!({
                            "error": {
                                "kind": "unknown_tool",
                                "message": format!("no tool named '{name}'"),
                                "available": self.tools.names(),
                            }
                        })
                        .to_string(),
                    },
                };
                stats.tool_calls += 1;
                self.observer.observe(&Event::ToolResult {
                    name: name.clone(),
                    ok: outcome.ok,
                    bytes: outcome.content.len(),
                });
                messages.push(Message::tool(&call.id, outcome.content));
            }
        };

        stats.elapsed_ms = budget.elapsed().as_millis() as u64;
        let status = Status::of(stop_reason);
        let result = RunResult {
            run_id,
            status,
            stop_reason,
            goal: goal.to_owned(),
            answer,
            error,
            messages,
            stats,
        };

        self.observer.observe(&Event::RunStopped {
            reason: stop_reason,
            status,
            stats,
        });

        // Best-effort host notification; a failed emit must not change the
        // outcome the caller already earned.
        let _ = self.host.emit(
            FINISHED_TOPIC,
            json!({
                "runId": result.run_id,
                "status": result.status,
                "stopReason": result.stop_reason,
                "modelCalls": result.stats.model_calls,
                "toolCalls": result.stats.tool_calls,
                "elapsedMs": result.stats.elapsed_ms,
            }),
        );

        result
    }
}

/// `run-<epoch millis>-<pid>`: sortable, unique per process, no dependency.
/// Monotonic counter that keeps ids distinct within one millisecond.
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A run identifier that sorts by start time.
///
/// The timestamp alone is not enough: two runs started in the same millisecond
/// would share an id and, because the id names the transcript directory, the
/// second would overwrite the first. The counter is zero-padded so that string
/// order still matches start order past nine runs in a millisecond.
fn new_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("run-{millis}-{}-{seq:06}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, Result};
    use crate::host::ExtensionSummary;
    use crate::message::ToolCall;
    use crate::model::{ModelResponse, ToolSchema, Usage};
    use crate::tools::{ToolSpec, ToolTarget};
    use serde_json::Value;
    use std::collections::VecDeque;

    struct FakeModel {
        turns: VecDeque<Result<ModelResponse>>,
        requests: Vec<ModelRequest>,
    }

    impl FakeModel {
        fn new(turns: Vec<Result<ModelResponse>>) -> Self {
            Self {
                turns: turns.into(),
                requests: Vec::new(),
            }
        }

        fn answer(text: &str) -> Result<ModelResponse> {
            Ok(ModelResponse {
                message: Message::Assistant {
                    content: Some(text.to_owned()),
                    tool_calls: Vec::new(),
                },
                usage: None,
            })
        }

        fn calls(name: &str) -> Result<ModelResponse> {
            Ok(ModelResponse {
                message: Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall::new("call_1", name, "{}")],
                },
                usage: Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                }),
            })
        }
    }

    impl Model for FakeModel {
        fn label(&self) -> String {
            "fake".to_owned()
        }
        fn complete(&mut self, request: &ModelRequest) -> Result<ModelResponse> {
            self.requests.push(request.clone());
            self.turns
                .pop_front()
                .unwrap_or_else(|| Err(Error::Model("fake model exhausted".to_owned())))
        }
    }

    struct FakeHost {
        calls: Vec<(String, String, Value)>,
        fail: Option<String>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                fail: None,
            }
        }
    }

    impl HostApi for FakeHost {
        fn info(&mut self) -> Result<Value> {
            Ok(json!({}))
        }
        fn list_extensions(&mut self) -> Result<Vec<ExtensionSummary>> {
            Ok(Vec::new())
        }
        fn call_extension(&mut self, id: &str, method: &str, params: Value) -> Result<Value> {
            self.calls.push((id.to_owned(), method.to_owned(), params));
            match &self.fail {
                Some(message) => Err(Error::Host(message.clone())),
                None => Ok(json!({"ok": true})),
            }
        }
        fn emit(&mut self, _topic: &str, _payload: Value) -> Result<usize> {
            Ok(1)
        }
    }

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.insert(
            ToolSpec {
                schema: ToolSchema {
                    name: "ping".to_owned(),
                    description: "ping the host".to_owned(),
                    parameters: json!({"type": "object"}),
                },
                target: ToolTarget {
                    extension: "toolbox".to_owned(),
                    method: "ping".to_owned(),
                },
            },
            "toolbox",
        );
        registry
    }

    fn run(
        model: &mut FakeModel,
        host: &mut FakeHost,
        tools: &ToolRegistry,
        limits: Limits,
    ) -> (RunResult, Recorder) {
        let mut recorder = Recorder::default();
        let result = Runner::new(model, host, tools, &mut recorder)
            .with_limits(limits)
            .run("do the thing");
        (result, recorder)
    }

    fn generous() -> Limits {
        Limits {
            max_model_calls: 10,
            max_tool_calls: 10,
            timeout: None,
        }
    }

    #[test]
    fn loop_executes_tools_then_answers() {
        let mut model = FakeModel::new(vec![FakeModel::calls("ping"), FakeModel::answer("done")]);
        let mut host = FakeHost::new();
        let tools = registry();

        let (result, recorder) = run(&mut model, &mut host, &tools, generous());

        assert_eq!(result.status, Status::Completed);
        assert_eq!(result.stop_reason, StopReason::Completed);
        assert_eq!(result.answer.as_deref(), Some("done"));
        assert_eq!(result.stats.model_calls, 2);
        assert_eq!(result.stats.tool_calls, 1);
        assert_eq!(result.stats.prompt_tokens, 7);
        assert_eq!(host.calls.len(), 1);
        assert_eq!(host.calls[0].0, "toolbox");

        // system, user, assistant(tool call), tool observation, assistant(answer)
        assert_eq!(result.messages.len(), 5);
        assert!(matches!(result.messages[0], Message::System { .. }));
        assert!(matches!(result.messages[4], Message::Assistant { .. }));
        assert_eq!(result.exit_code(), 0);

        // The second request must carry the observation back to the model.
        assert_eq!(model.requests.len(), 2);
        let last = model.requests.last().expect("request");
        assert!(matches!(last.messages.last(), Some(Message::Tool { .. })));

        assert!(
            recorder
                .events()
                .iter()
                .any(|event| matches!(event, Event::ToolResult { ok: true, .. }))
        );
    }

    #[test]
    fn model_budget_stops_a_looping_model() {
        let mut model = FakeModel::new(vec![
            FakeModel::calls("ping"),
            FakeModel::calls("ping"),
            FakeModel::calls("ping"),
        ]);
        let mut host = FakeHost::new();
        let tools = registry();

        let limits = Limits {
            max_model_calls: 2,
            max_tool_calls: 10,
            timeout: None,
        };
        let (result, _) = run(&mut model, &mut host, &tools, limits);

        assert_eq!(result.stop_reason, StopReason::ModelBudget);
        assert_eq!(result.status, Status::Incomplete);
        assert_eq!(result.stats.model_calls, 2);
        assert_eq!(result.exit_code(), 1);
        assert_eq!(
            model.requests.len(),
            2,
            "the third turn must never be requested"
        );
    }

    #[test]
    fn tool_budget_refuses_a_turn_that_does_not_fit() {
        let mut model = FakeModel::new(vec![Ok(ModelResponse {
            message: Message::Assistant {
                content: None,
                tool_calls: vec![
                    ToolCall::new("a", "ping", "{}"),
                    ToolCall::new("b", "ping", "{}"),
                ],
            },
            usage: None,
        })]);
        let mut host = FakeHost::new();
        let tools = registry();

        let limits = Limits {
            max_model_calls: 5,
            max_tool_calls: 1,
            timeout: None,
        };
        let (result, _) = run(&mut model, &mut host, &tools, limits);

        assert_eq!(result.stop_reason, StopReason::ToolBudget);
        assert!(host.calls.is_empty(), "an unaffordable turn runs nothing");
        assert_eq!(result.stats.tool_calls, 0);
    }

    #[test]
    fn unknown_tool_becomes_an_observation_and_the_loop_continues() {
        let mut model = FakeModel::new(vec![
            FakeModel::calls("does_not_exist"),
            FakeModel::answer("recovered"),
        ]);
        let mut host = FakeHost::new();
        let tools = registry();

        let (result, _) = run(&mut model, &mut host, &tools, generous());

        assert_eq!(result.status, Status::Completed);
        assert_eq!(result.answer.as_deref(), Some("recovered"));
        assert!(host.calls.is_empty());
        let observation = result
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("observation");
        assert!(observation.contains("unknown_tool"));
        assert!(
            observation.contains("ping"),
            "the model is told what exists"
        );
    }

    #[test]
    fn host_failure_is_an_observation_not_a_stop() {
        let mut model = FakeModel::new(vec![FakeModel::calls("ping"), FakeModel::answer("ok")]);
        let mut host = FakeHost::new();
        host.fail = Some("extension 'toolbox' lacks capability 'storage.write'".to_owned());
        let tools = registry();

        let (result, _) = run(&mut model, &mut host, &tools, generous());

        assert_eq!(result.status, Status::Completed);
        let observation = result
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("observation");
        assert!(observation.contains("storage.write"));
    }

    #[test]
    fn model_error_ends_the_run_with_its_message() {
        let mut model = FakeModel::new(vec![Err(Error::Model("503 from endpoint".to_owned()))]);
        let mut host = FakeHost::new();
        let tools = registry();

        let (result, _) = run(&mut model, &mut host, &tools, generous());

        assert_eq!(result.stop_reason, StopReason::ModelError);
        assert_eq!(result.status, Status::Incomplete);
        assert!(result.error.expect("error text").contains("503"));
    }

    #[test]
    fn empty_answer_is_not_success() {
        let mut model = FakeModel::new(vec![FakeModel::answer("   ")]);
        let mut host = FakeHost::new();
        let tools = registry();

        let (result, _) = run(&mut model, &mut host, &tools, generous());

        assert_eq!(result.stop_reason, StopReason::EmptyAnswer);
        assert_eq!(result.status, Status::Incomplete);
        assert!(result.answer.is_none());
    }

    #[test]
    fn deadline_stops_before_any_model_call() {
        let mut model = FakeModel::new(vec![FakeModel::answer("never reached")]);
        let mut host = FakeHost::new();
        let tools = registry();

        let limits = Limits {
            max_model_calls: 5,
            max_tool_calls: 5,
            timeout: Some(std::time::Duration::ZERO),
        };
        let (result, _) = run(&mut model, &mut host, &tools, limits);

        assert_eq!(result.stop_reason, StopReason::Deadline);
        assert!(model.requests.is_empty());
    }

    #[test]
    fn system_prompt_can_be_replaced() {
        let mut model = FakeModel::new(vec![FakeModel::answer("ok")]);
        let mut host = FakeHost::new();
        let tools = registry();
        let mut recorder = Recorder::default();

        Runner::new(&mut model, &mut host, &tools, &mut recorder)
            .with_system_prompt("be terse")
            .with_limits(generous())
            .run("go");

        let request = model.requests.first().expect("request");
        assert_eq!(request.messages[0].content(), Some("be terse"));
        assert_eq!(request.tools.len(), 1, "tools are advertised every turn");
    }

    #[test]
    fn run_ids_are_unique_and_sortable() {
        let first = new_run_id();
        let second = new_run_id();
        assert!(first.starts_with("run-"));
        assert_ne!(first, second);
    }
}
