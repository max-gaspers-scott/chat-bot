//! Agent loop: `run_task`.
//!
//! Drives a multi-turn conversation against a rig [`DynModel<Completion>`],
//! handling tool calls, approval, cancellation, and transcript logging.
//!
//! # Usage
//!
//! ```no_run
//! use harness::agent_loop::{run_task, LoopConfig};
//! use harness::memory::InMemoryConversation;
//! use harness::events::{HarnessEvent, auto_approve};
//! use tokio::sync::mpsc;
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // (model would come from ClientBuilder::build() in real usage)
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rig_core::completion::message::{
    ToolResult, ToolResultContent,
};
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::message::Message;
use rig_core::operation::Completion;
use rig_core::DynModel;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::events::{ApprovalDecision, ApprovalHook, HarnessEvent};
use crate::memory::Memory;
use crate::transcript::Transcript;

// ── TaskOutcome ───────────────────────────────────────────────────────────────

/// The result of a completed `run_task` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The model produced a final text answer.
    Completed { output: String },
    /// `max_turns` was reached without a terminal answer.
    MaxTurnsReached,
    /// A cancellation token was signalled.
    Cancelled,
}

// ── ToolRegistry ─────────────────────────────────────────────────────────────

/// A registered in-process tool.
pub struct ToolEntry {
    /// Rig tool definition (name, description, JSON schema).
    pub definition: ToolDefinition,
    /// Whether the tool requires user approval before each call.
    pub requires_approval: bool,
    /// The executor: receives JSON arguments, returns a string result.
    pub executor: Box<dyn Fn(serde_json::Value) -> Result<String> + Send + Sync + 'static>,
}

impl std::fmt::Debug for ToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEntry")
            .field("name", &self.definition.name)
            .field("requires_approval", &self.requires_approval)
            .finish()
    }
}

// ── LoopConfig ────────────────────────────────────────────────────────────────

/// Configuration for a single `run_task` call.
pub struct LoopConfig {
    /// Maximum number of model turns (not counting tool-result turns).
    /// Defaults to 50.
    pub max_turns: usize,

    /// System prompt prepended to every request.  Can be overridden to inject
    /// `AGENTS.md` content at startup.
    pub system_prompt: Option<String>,

    /// Tools available to the model.
    pub tools: Vec<ToolEntry>,

    /// Approval hook called before executing any tool with
    /// `requires_approval = true`.
    pub approval: ApprovalHook,

    /// When set, a session transcript is written here.
    /// Format: `<dir>/<session_id>.jsonl`.
    pub transcript_dir: Option<PathBuf>,

    /// Session identifier used as the transcript filename stem.
    pub session_id: String,

    /// Model identifier override to pass to rig (e.g. `"gemini-2.5-flash"`).
    /// When `None`, the model's own default is used.
    pub model_id: Option<String>,
}

impl Default for LoopConfig {
    fn default() -> Self {
        use crate::events::auto_approve;
        Self {
            max_turns: 50,
            system_prompt: None,
            tools: Vec::new(),
            approval: auto_approve(),
            transcript_dir: None,
            session_id: "default".into(),
            model_id: None,
        }
    }
}

impl LoopConfig {
    /// Load `AGENTS.md` from `workspace_root` (if it exists) and prepend it
    /// to the system prompt.
    pub async fn with_agents_md(mut self, workspace_root: impl AsRef<std::path::Path>) -> Self {
        let path = workspace_root.as_ref().join("AGENTS.md");
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            let header = format!(
                "# Agent instructions (from AGENTS.md)\n\n{}\n\n",
                content.trim()
            );
            self.system_prompt = Some(match self.system_prompt.take() {
                Some(existing) => format!("{header}{existing}"),
                None => header,
            });
        }
        self
    }
}

// ── run_task ─────────────────────────────────────────────────────────────────

/// Run the agent loop for one user task.
///
/// - `model`: a rig `DynModel<Completion>` (real or mock).
/// - `task`: the initial user message.
/// - `memory`: mutable conversation history (extended in place; call
///   `run_task` again against the same memory for follow-up messages).
/// - `events`: channel for pushing events to the frontend.
/// - `cancel`: token that stops the loop mid-turn.
/// - `config`: loop configuration.
///
/// Returns [`TaskOutcome`] on success, or an error if the model call or tool
/// execution fails unexpectedly.
pub async fn run_task(
    model: &DynModel<Completion>,
    task: impl Into<String>,
    memory: &mut dyn Memory,
    events: &mpsc::Sender<HarnessEvent>,
    cancel: CancellationToken,
    config: &LoopConfig,
) -> Result<TaskOutcome> {
    let task = task.into();

    // Optional transcript.
    let transcript: Option<Transcript> = match &config.transcript_dir {
        Some(dir) => Some(
            Transcript::open_in(dir, &config.session_id)
                .await
                .context("could not open transcript")?,
        ),
        None => None,
    };

    // Append the user task to memory.
    let user_msg = Message::user(task);
    memory.append(user_msg.clone());
    log_message(&transcript, &user_msg).await;

    // Build the set of "always allow" tool names accumulated during this session.
    let mut always_allowed: HashSet<String> = HashSet::new();

    for turn in 0..config.max_turns {
        // ── Check cancellation before each turn ──────────────────────────
        if cancel.is_cancelled() {
            let ev = HarnessEvent::Cancelled;
            send_event(events, ev.clone()).await;
            log_event(&transcript, &ev).await;
            return Ok(TaskOutcome::Cancelled);
        }

        // ── Build the completion request ─────────────────────────────────
        let history = memory.load();
        let tool_defs: Vec<ToolDefinition> = config
            .tools
            .iter()
            .map(|t| t.definition.clone())
            .collect();

        // The last message in history is the prompt; everything before it
        // is the conversation context.
        let mut request = if history.len() == 1 {
            CompletionRequest::new(history[0].clone())
        } else {
            let prompt = history.last().unwrap().clone();
            let prior = history[..history.len() - 1].to_vec();
            CompletionRequest::new(prompt).messages(prior)
        };

        // Inject system prompt if configured.
        if let Some(sys) = &config.system_prompt {
            request = request.preamble(sys.clone());
        }

        // Add tools.
        if !tool_defs.is_empty() {
            request = request.tools(tool_defs);
        }

        // Add model override.
        if let Some(model_id) = &config.model_id {
            request = request.model::<String>(Some(model_id.clone()));
        }

        // ── Call the model ────────────────────────────────────────────────
        debug!(turn, "calling model");

        // Use tokio::select! so cancellation interrupts the in-flight call.
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let ev = HarnessEvent::Cancelled;
                send_event(events, ev.clone()).await;
                log_event(&transcript, &ev).await;
                return Ok(TaskOutcome::Cancelled);
            }
            result = model.call(request) => {
                result.map_err(|e| anyhow::anyhow!("model call failed: {e}"))?
            }
        };

        // ── Collect tool calls and text from the response ─────────────────
        let tool_calls: Vec<_> = response.tool_calls().cloned().collect();
        let text = response.text();

        // The model's assistant turn goes into memory.
        if let Some(assistant_msg) = response.message() {
            memory.append(assistant_msg.clone());
            log_message(&transcript, &assistant_msg).await;
        }

        // Emit text deltas (full turn text as one event for non-streaming).
        if !text.is_empty() {
            let ev = HarnessEvent::TextDelta { delta: text.clone() };
            send_event(events, ev.clone()).await;
            log_event(&transcript, &ev).await;
        }

        // ── Turn finished ─────────────────────────────────────────────────
        let turn_ev = HarnessEvent::TurnFinished { turn };
        send_event(events, turn_ev.clone()).await;
        log_event(&transcript, &turn_ev).await;

        // ── If no tool calls, we're done ──────────────────────────────────
        if tool_calls.is_empty() {
            let ev = HarnessEvent::TaskComplete { output: text.clone() };
            send_event(events, ev.clone()).await;
            log_event(&transcript, &ev).await;
            return Ok(TaskOutcome::Completed { output: text });
        }

        // ── Dispatch tool calls ───────────────────────────────────────────
        let mut tool_results: Vec<ToolResult> = Vec::new();

        for call in &tool_calls {
            // Cancellation check before each tool.
            if cancel.is_cancelled() {
                let ev = HarnessEvent::Cancelled;
                send_event(events, ev.clone()).await;
                log_event(&transcript, &ev).await;
                return Ok(TaskOutcome::Cancelled);
            }

            let call_id_str = call.id.wire().into_owned();
            let name = call.function.name.as_ref().to_string();
            let args = call.function.arguments.clone();

            // Emit tool-call-started event.
            let started_ev = HarnessEvent::ToolCallStarted {
                call_id: call_id_str.clone(),
                name: name.clone(),
                arguments: serde_json::to_string_pretty(&args).unwrap_or_default(),
            };
            send_event(events, started_ev.clone()).await;
            log_event(&transcript, &started_ev).await;

            // Find the tool.
            let entry = config.tools.iter().find(|t| t.definition.name == name);

            let result_text = match entry {
                None => format!("Error: unknown tool `{name}`"),
                Some(entry) => {
                    // Approval check.
                    let needs_approval =
                        entry.requires_approval && !always_allowed.contains(&name);

                    if needs_approval {
                        let preview = format!("Tool: {name}\nArgs: {args}");
                        let approval_ev = HarnessEvent::ApprovalRequested {
                            call_id: call_id_str.clone(),
                            name: name.clone(),
                            preview: preview.clone(),
                        };
                        send_event(events, approval_ev.clone()).await;
                        log_event(&transcript, &approval_ev).await;

                        match (config.approval)(call, &preview) {
                            ApprovalDecision::Denied { reason } => {
                                let msg = reason
                                    .as_deref()
                                    .unwrap_or("User denied the tool call.")
                                    .to_string();
                                format!("Tool call denied: {msg}")
                            }
                            ApprovalDecision::Approved { always_allow } => {
                                if always_allow {
                                    always_allowed.insert(name.clone());
                                }
                                match (entry.executor)(args) {
                                    Ok(out) => out,
                                    Err(e) => format!("Tool error: {e:#}"),
                                }
                            }
                        }
                    } else {
                        match (entry.executor)(args) {
                            Ok(out) => out,
                            Err(e) => format!("Tool error: {e:#}"),
                        }
                    }
                }
            };

            // Emit tool-call-finished event.
            let finished_ev = HarnessEvent::ToolCallFinished {
                call_id: call_id_str.clone(),
                name: name.clone(),
                output: result_text.clone(),
            };
            send_event(events, finished_ev.clone()).await;
            log_event(&transcript, &finished_ev).await;

            tool_results.push(call.result(vec![ToolResultContent::text(result_text)]));
        }

        // Append the tool results as a user message (never split from the
        // assistant turn that requested them).
        let results_msg = Message::tool_results(tool_results);
        memory.append(results_msg.clone());
        log_message(&transcript, &results_msg).await;
    }

    // Max turns reached.
    let ev = HarnessEvent::MaxTurnsReached {
        max_turns: config.max_turns,
    };
    send_event(events, ev.clone()).await;
    log_event(&transcript, &ev).await;
    Ok(TaskOutcome::MaxTurnsReached)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn send_event(tx: &mpsc::Sender<HarnessEvent>, event: HarnessEvent) {
    // A closed receiver means the frontend has disconnected — that's fine.
    let _ = tx.send(event).await;
}

async fn log_message(transcript: &Option<Transcript>, message: &Message) {
    if let Some(t) = transcript {
        if let Err(e) = t.log_message(message).await {
            tracing::warn!(%e, "transcript write failed");
        }
    }
}

async fn log_event(transcript: &Option<Transcript>, event: &HarnessEvent) {
    if let Some(t) = transcript {
        if let Err(e) = t.log_event(event).await {
            tracing::warn!(%e, "transcript write failed");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryConversation;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    async fn collect_events(mut rx: mpsc::Receiver<HarnessEvent>) -> Vec<HarnessEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn plain_reply() {
        let model = MockCompletionModel::from_turns([MockTurn::text("hello world")]).erase();
        let (tx, rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let config = LoopConfig { max_turns: 5, ..LoopConfig::default() };

        let outcome = run_task(&model, "hi", &mut mem, &tx, no_cancel(), &config)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            TaskOutcome::Completed { output: "hello world".into() }
        );

        // Memory should have user + assistant messages.
        let history = mem.load();
        assert_eq!(history.len(), 2);

        let events = collect_events(rx).await;
        let has_text_delta = events
            .iter()
            .any(|e| matches!(e, HarnessEvent::TextDelta { delta } if delta == "hello world"));
        assert!(has_text_delta, "expected TextDelta event");
    }

    #[tokio::test]
    async fn tool_call_round_trip() {
        use rig_core::completion::ToolDefinition;

        // Turn 1: model calls "echo", Turn 2: model replies with final text.
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call-1", "echo", serde_json::json!({"text": "ping"})),
            MockTurn::text("pong"),
        ])
        .erase();

        let (tx, rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let config = LoopConfig {
            max_turns: 5,
            tools: vec![ToolEntry {
                definition: ToolDefinition {
                    name: "echo".into(),
                    description: "Echo the input.".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                },
                requires_approval: false,
                executor: Box::new(|args| {
                    let text = args["text"].as_str().unwrap_or("").to_string();
                    Ok(text)
                }),
            }],
            ..LoopConfig::default()
        };

        let outcome = run_task(&model, "ping", &mut mem, &tx, no_cancel(), &config)
            .await
            .unwrap();

        assert_eq!(outcome, TaskOutcome::Completed { output: "pong".into() });

        let events = collect_events(rx).await;
        let started = events.iter().any(|e| matches!(e, HarnessEvent::ToolCallStarted { name, .. } if name == "echo"));
        let finished = events.iter().any(|e| matches!(e, HarnessEvent::ToolCallFinished { name, .. } if name == "echo"));
        assert!(started, "expected ToolCallStarted");
        assert!(finished, "expected ToolCallFinished");
    }

    #[tokio::test]
    async fn approval_granted() {
        use crate::events::auto_approve;
        use rig_core::completion::ToolDefinition;

        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("c1", "guarded", serde_json::json!({})),
            MockTurn::text("done"),
        ])
        .erase();

        let (tx, _rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let config = LoopConfig {
            max_turns: 5,
            tools: vec![ToolEntry {
                definition: ToolDefinition {
                    name: "guarded".into(),
                    description: "Needs approval.".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                },
                requires_approval: true,
                executor: Box::new(|_| Ok("executed".into())),
            }],
            approval: auto_approve(),
            ..LoopConfig::default()
        };

        let outcome = run_task(&model, "go", &mut mem, &tx, no_cancel(), &config)
            .await
            .unwrap();

        assert_eq!(outcome, TaskOutcome::Completed { output: "done".into() });
    }

    #[tokio::test]
    async fn approval_denied() {
        use crate::events::auto_deny;
        use rig_core::completion::ToolDefinition;

        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("c1", "dangerous", serde_json::json!({})),
            MockTurn::text("ok"),
        ])
        .erase();

        let (tx, rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let config = LoopConfig {
            max_turns: 5,
            tools: vec![ToolEntry {
                definition: ToolDefinition {
                    name: "dangerous".into(),
                    description: "Dangerous.".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                },
                requires_approval: true,
                executor: Box::new(|_| panic!("should not execute")),
            }],
            approval: auto_deny("not allowed"),
            ..LoopConfig::default()
        };

        let outcome = run_task(&model, "do it", &mut mem, &tx, no_cancel(), &config)
            .await
            .unwrap();

        // Loop should continue after denial (model gets "Tool call denied").
        assert_eq!(outcome, TaskOutcome::Completed { output: "ok".into() });

        let events = collect_events(rx).await;
        let denied = events.iter().any(|e| {
            matches!(e, HarnessEvent::ApprovalRequested { name, .. } if name == "dangerous")
        });
        assert!(denied, "expected ApprovalRequested event");
    }

    #[tokio::test]
    async fn max_turns() {
        // Model always calls a tool → should hit max_turns.
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("c1", "echo", serde_json::json!({"text": "a"})),
            MockTurn::tool_call("c2", "echo", serde_json::json!({"text": "b"})),
            MockTurn::tool_call("c3", "echo", serde_json::json!({"text": "c"})),
        ])
        .erase();

        let (tx, rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let config = LoopConfig {
            max_turns: 2,
            tools: vec![ToolEntry {
                definition: rig_core::completion::ToolDefinition {
                    name: "echo".into(),
                    description: "Echo.".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
                },
                requires_approval: false,
                executor: Box::new(|args| Ok(args["text"].as_str().unwrap_or("").to_string())),
            }],
            ..LoopConfig::default()
        };

        let outcome = run_task(&model, "go", &mut mem, &tx, no_cancel(), &config)
            .await
            .unwrap();

        assert_eq!(outcome, TaskOutcome::MaxTurnsReached);

        let events = collect_events(rx).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, HarnessEvent::MaxTurnsReached { max_turns: 2 })));
    }

    #[tokio::test]
    async fn cancellation() {
        let model = MockCompletionModel::from_turns([MockTurn::text("should not reach")]).erase();
        let (tx, rx) = mpsc::channel(32);
        let mut mem = InMemoryConversation::new();
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before the loop starts

        let config = LoopConfig { max_turns: 5, ..LoopConfig::default() };

        let outcome = run_task(&model, "hi", &mut mem, &tx, cancel, &config)
            .await
            .unwrap();

        assert_eq!(outcome, TaskOutcome::Cancelled);
        let events = collect_events(rx).await;
        assert!(events.iter().any(|e| matches!(e, HarnessEvent::Cancelled)));
    }
}
