//! Budgets and the wall-clock deadline.
//!
//! The loop must terminate even when the model keeps calling tools. Three
//! independent limits do that, and they are checked *before* spending, so a run
//! never overshoots: model calls, tool calls, and elapsed time. Token budgets are
//! deliberately absent — usage reporting is optional in the protocol, so a token
//! limit would silently not apply against endpoints that omit `usage`.

use std::time::{Duration, Instant};

/// Loop limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Maximum assistant turns requested from the model.
    pub max_model_calls: u32,
    /// Maximum tool invocations across the whole run.
    pub max_tool_calls: u32,
    /// Wall-clock budget for the run; `None` disables the deadline.
    pub timeout: Option<Duration>,
}

impl Default for Limits {
    /// Matches the sibling agent's defaults: 20 turns, 50 tool calls, 5 minutes.
    fn default() -> Self {
        Self {
            max_model_calls: 20,
            max_tool_calls: 50,
            timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// Why a budget refused to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    /// `max_model_calls` reached.
    ModelCalls,
    /// `max_tool_calls` reached.
    ToolCalls,
    /// The deadline passed.
    Deadline,
}

impl Exhausted {
    /// Stable label for events and JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ModelCalls => "model_calls",
            Self::ToolCalls => "tool_calls",
            Self::Deadline => "deadline",
        }
    }
}

/// Usage counter with a hard ceiling.
#[derive(Debug, Clone)]
pub struct Budget {
    limits: Limits,
    model_calls: u32,
    tool_calls: u32,
    started: Instant,
}

impl Budget {
    /// Starts the clock.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            model_calls: 0,
            tool_calls: 0,
            started: Instant::now(),
        }
    }

    /// Reserves one model turn and returns its 1-based index.
    pub fn begin_model_call(&mut self) -> std::result::Result<u32, Exhausted> {
        self.check_deadline()?;
        if self.model_calls >= self.limits.max_model_calls {
            return Err(Exhausted::ModelCalls);
        }
        self.model_calls += 1;
        Ok(self.model_calls)
    }

    /// Reserves `count` tool invocations.
    ///
    /// All-or-nothing: a turn that requests more calls than remain is refused as
    /// a whole, so the transcript cannot end up with unanswered tool calls.
    pub fn charge_tools(&mut self, count: usize) -> std::result::Result<(), Exhausted> {
        self.check_deadline()?;
        let count = count as u32;
        if self.tool_calls.saturating_add(count) > self.limits.max_tool_calls {
            return Err(Exhausted::ToolCalls);
        }
        self.tool_calls += count;
        Ok(())
    }

    /// Fails once the wall-clock budget is spent.
    pub fn check_deadline(&self) -> std::result::Result<(), Exhausted> {
        match self.limits.timeout {
            Some(timeout) if self.started.elapsed() >= timeout => Err(Exhausted::Deadline),
            _ => Ok(()),
        }
    }

    /// Model turns spent.
    pub fn model_calls(&self) -> u32 {
        self.model_calls
    }

    /// Tool invocations spent.
    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// Time since the run started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Time left before the deadline, when one is set.
    pub fn remaining(&self) -> Option<Duration> {
        self.limits
            .timeout
            .map(|timeout| timeout.saturating_sub(self.started.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(model: u32, tools: u32) -> Limits {
        Limits {
            max_model_calls: model,
            max_tool_calls: tools,
            timeout: None,
        }
    }

    #[test]
    fn model_calls_are_counted_and_capped() {
        let mut budget = Budget::new(limits(2, 10));
        assert_eq!(budget.begin_model_call(), Ok(1));
        assert_eq!(budget.begin_model_call(), Ok(2));
        assert_eq!(budget.begin_model_call(), Err(Exhausted::ModelCalls));
        assert_eq!(budget.model_calls(), 2);
    }

    #[test]
    fn tool_charging_is_all_or_nothing() {
        let mut budget = Budget::new(limits(5, 3));
        assert_eq!(budget.charge_tools(2), Ok(()));
        assert_eq!(budget.charge_tools(2), Err(Exhausted::ToolCalls));
        assert_eq!(budget.tool_calls(), 2, "a refused charge must not spend");
    }

    #[test]
    fn expired_deadline_refuses_both_kinds_of_spend() {
        let mut budget = Budget::new(Limits {
            max_model_calls: 5,
            max_tool_calls: 5,
            timeout: Some(Duration::ZERO),
        });
        assert_eq!(budget.begin_model_call(), Err(Exhausted::Deadline));
        assert_eq!(budget.charge_tools(1), Err(Exhausted::Deadline));
    }

    #[test]
    fn remaining_time_shrinks() {
        let budget = Budget::new(Limits {
            max_model_calls: 1,
            max_tool_calls: 1,
            timeout: Some(Duration::from_secs(60)),
        });
        let remaining = budget.remaining().expect("deadline");
        assert!(remaining <= Duration::from_secs(60));
        assert!(remaining > Duration::from_secs(50));
    }

    #[test]
    fn exhausted_labels_are_stable() {
        assert_eq!(Exhausted::ModelCalls.as_str(), "model_calls");
        assert_eq!(Exhausted::ToolCalls.as_str(), "tool_calls");
        assert_eq!(Exhausted::Deadline.as_str(), "deadline");
    }
}
