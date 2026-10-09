//! Events out (model → frontend) and commands in (frontend → loop).
//!
//! Frontends receive a stream of [`HarnessEvent`] values on a
//! `tokio::sync::mpsc` channel and send [`Command`] values in on a separate
//! channel.  The agent loop bridges between the two.

use serde::{Deserialize, Serialize};

use rig_core::completion::message::ToolCall;

// ── HarnessEvent ─────────────────────────────────────────────────────────────

/// An event produced by the agent loop and pushed to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// A delta of assistant text (streamed token by token, or full turn for
    /// non-streaming providers).
    TextDelta {
        /// Incremental text content.
        delta: String,
    },

    /// The model has requested a tool call.  Fires before the call is
    /// dispatched (or before approval is sought).
    ToolCallStarted {
        /// Unique call ID assigned by the provider.
        call_id: String,
        /// Tool name.
        name: String,
        /// JSON arguments as a pretty-printed string.
        arguments: String,
    },

    /// A tool call has finished executing.
    ToolCallFinished {
        /// The same call ID from [`HarnessEvent::ToolCallStarted`].
        call_id: String,
        /// Tool name.
        name: String,
        /// Output returned by the tool (possibly truncated).
        output: String,
    },

    /// The loop is waiting for the user to approve or deny a tool call before
    /// executing it.
    ApprovalRequested {
        /// Unique call ID.
        call_id: String,
        /// Tool name.
        name: String,
        /// Human-readable description of what the tool will do (e.g. a diff
        /// preview for `write_file`, the command for `bash`).
        preview: String,
    },

    /// An error occurred.  The loop may or may not have stopped.
    Error {
        /// Human-readable error message.
        message: String,
        /// `true` when the error is fatal and the loop has stopped.
        fatal: bool,
    },

    /// The current turn has finished.  Fired after the assistant's reply
    /// (and any tool calls) are done.
    TurnFinished {
        /// Zero-based turn index.
        turn: usize,
    },

    /// The task has completed (model stopped calling tools and produced a
    /// final text reply).
    TaskComplete {
        /// The final text reply.
        output: String,
    },

    /// The task was cancelled via a [`Command::Cancel`] or
    /// [`tokio_util::sync::CancellationToken`].
    Cancelled,

    /// `max_turns` was reached without a terminal answer.
    MaxTurnsReached {
        /// The limit that was hit.
        max_turns: usize,
    },
}

// ── Command ───────────────────────────────────────────────────────────────────

/// A command sent from the frontend into the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Send a new user message to the model.
    UserMessage {
        /// The message text.
        text: String,
    },

    /// Approve a pending tool call (identified by `call_id`).
    Approve {
        /// The call ID from the [`HarnessEvent::ApprovalRequested`] event.
        call_id: String,
        /// When `true`, the tool is always allowed for the rest of the session.
        always_allow: bool,
    },

    /// Deny a pending tool call (identified by `call_id`).
    Deny {
        /// The call ID from the [`HarnessEvent::ApprovalRequested`] event.
        call_id: String,
        /// Optional reason to feed back to the model.
        reason: Option<String>,
    },

    /// Cancel the running task.
    Cancel,
}

// ── ApprovalDecision ─────────────────────────────────────────────────────────

/// The result of an approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The tool call is allowed to proceed.
    Approved {
        /// When `true`, the tool should be always-allowed for the session.
        always_allow: bool,
    },
    /// The tool call is denied.
    Denied {
        /// Optional reason for the model.
        reason: Option<String>,
    },
}

// ── ApprovalHook ─────────────────────────────────────────────────────────────

/// A callback invoked by the agent loop before executing a tool that requires
/// approval.
///
/// Return [`ApprovalDecision::Approved`] to allow the call or
/// [`ApprovalDecision::Denied`] to block it.
///
/// The default hook (returned by [`auto_approve`]) approves everything
/// without prompting.
pub type ApprovalHook =
    Box<dyn Fn(&ToolCall, &str) -> ApprovalDecision + Send + Sync + 'static>;

/// Build an [`ApprovalHook`] that always approves.
pub fn auto_approve() -> ApprovalHook {
    Box::new(|_call, _preview| ApprovalDecision::Approved { always_allow: false })
}

/// Build an [`ApprovalHook`] that always denies with an optional reason.
pub fn auto_deny(reason: impl Into<String> + Clone + Send + Sync + 'static) -> ApprovalHook {
    Box::new(move |_call, _preview| ApprovalDecision::Denied {
        reason: Some(reason.clone().into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_returns_approved() {
        use rig_core::completion::message::{ToolFunction, ToolName};
        let hook = auto_approve();
        let call = ToolCall::from_wire(
            "call-1",
            ToolFunction::new(ToolName::new("bash").unwrap(), serde_json::json!({})),
        );
        let decision = hook(&call, "echo hi");
        assert!(matches!(decision, ApprovalDecision::Approved { .. }));
    }

    #[test]
    fn auto_deny_returns_denied() {
        use rig_core::completion::message::{ToolFunction, ToolName};
        let hook = auto_deny("not allowed");
        let call = ToolCall::from_wire(
            "call-2",
            ToolFunction::new(ToolName::new("bash").unwrap(), serde_json::json!({})),
        );
        let decision = hook(&call, "rm -rf /");
        assert!(matches!(decision, ApprovalDecision::Denied { reason: Some(_) }));
    }

    #[test]
    fn harness_event_serialises() {
        let ev = HarnessEvent::TextDelta { delta: "hello".into() };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("text_delta"));
    }

    #[test]
    fn command_serialises() {
        let cmd = Command::Cancel;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("cancel"));
    }
}
