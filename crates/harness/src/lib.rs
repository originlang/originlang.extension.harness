//! OriginLang extension harness — the host's first plugin.
//!
//! # Where the boundary sits
//!
//! The harness ships as a `cdylib` and is loaded into the host process by the
//! host's dylib backend. It is a plugin, not a second binary and not a linked
//! library: the host does not depend on it, and adding it does not change the
//! host's size by a single byte until the package is installed.
//!
//! The boundary is the C ABI in `originlang-ext-plugin-abi`. Every host request
//! the loop makes travels through one function pointer, which lands on the same
//! dispatcher that enforces capabilities — so a plugin call is still checked,
//! timed and logged like any other.
//!
//! # What each module owns
//!
//! - [`tools`] turns `contributes.tools` from installed packages into a registry,
//!   so installing an extension is what makes a tool available.
//! - [`run`] is the loop: model turn, tool calls, observations, budget, stop.
//! - [`model`] is the completions boundary; [`http`] is the only place that
//!   performs network I/O, through the system `curl`.
//! - [`transcript`] persists a run so two runs can be compared later.
//! - [`host`] is the adapter from the plugin ABI to [`host::HostApi`].
//!
//! # Boundaries this crate refuses
//!
//! - It does not sandbox tools. Tool code runs in the host under the host's
//!   capability model; the harness only chooses which tool to call.
//! - It does not retry. A failed model call ends the run with a reason; retrying
//!   is the caller's policy, not the loop's.
//! - It does not stream. Partial tokens need a streaming transport, and the
//!   loop's unit of work is one complete turn.
//!
//! ```
//! use originlang_ext_harness::Limits;
//! let limits = Limits::default();
//! assert_eq!(limits.max_model_calls, 20);
//! ```

#![deny(missing_docs)]

pub mod budget;
pub mod error;
pub mod host;
pub mod http;
pub mod message;
pub mod model;
pub mod run;
pub mod tools;
pub mod transcript;

pub use budget::{Budget, Exhausted, Limits};
pub use error::{Error, Result};
pub use host::{ExtensionSummary, HostApi, PluginHost};
pub use message::{FunctionCall, Message, ToolCall};
pub use model::{
    Model, ModelRequest, ModelResponse, OpenAiConfig, OpenAiModel, ScriptedModel, ToolSchema, Usage,
};
pub use run::{
    DEFAULT_SYSTEM_PROMPT, Event, FINISHED_TOPIC, Observer, Recorder, RunResult, Runner, Silent,
    Stats, Status, StopReason,
};
pub use tools::{MAX_TOOL_RESULT_BYTES, ToolOutcome, ToolRegistry, ToolSpec, ToolTarget};
