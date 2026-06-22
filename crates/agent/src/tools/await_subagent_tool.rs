use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// Wait for a background sub-agent to finish, then return its result.
///
/// Pass the `session_id` from `spawn_agent_background`. This blocks until the
/// sub-agent reaches a terminal state (completed/failed/cancelled) or the optional
/// timeout elapses, then returns its final `output` (or `error`). Use this when you
/// need a background sub-agent's result before continuing; use `list_subagents` if
/// you only want a non-blocking status check.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct AwaitSubagentToolInput {
    /// The session ID of the background sub-agent to wait for.
    pub session_id: acp::SessionId,
    /// Maximum time to wait, in seconds (default 300). On timeout the sub-agent
    /// keeps running and its current status is returned.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum AwaitSubagentToolOutput {
    Finished {
        session_id: acp::SessionId,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TimedOut {
        session_id: acp::SessionId,
        status: String,
    },
    Error {
        error: String,
    },
}

impl From<AwaitSubagentToolOutput> for LanguageModelToolResultContent {
    fn from(output: AwaitSubagentToolOutput) -> Self {
        let value = match output {
            AwaitSubagentToolOutput::Finished {
                session_id,
                status,
                output,
                error,
            } => serde_json::json!({
                "session_id": session_id,
                "status": status,
                "output": output,
                "error": error,
            }),
            AwaitSubagentToolOutput::TimedOut { session_id, status } => serde_json::json!({
                "session_id": session_id,
                "status": status,
                "timed_out": true,
            }),
            AwaitSubagentToolOutput::Error { error } => serde_json::json!({ "error": error }),
        };
        serde_json::to_string(&value)
            .unwrap_or_else(|e| format!("Failed to serialize await_subagent output: {e}"))
            .into()
    }
}

/// Tool that waits for a background sub-agent to finish.
pub struct AwaitSubagentTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl AwaitSubagentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for AwaitSubagentTool {
    type Input = AwaitSubagentToolInput;
    type Output = AwaitSubagentToolOutput;

    const NAME: &'static str = "await_subagent";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Waiting for background sub-agent".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| AwaitSubagentToolOutput::Error {
                    error: e.to_string(),
                })?;

            let session_id = input.session_id;
            let timeout =
                Duration::from_secs(input.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS));
            let deadline = Instant::now() + timeout;

            loop {
                let summary = cx
                    .update(|cx| self.environment.list_subagents(cx))
                    .into_iter()
                    .find(|summary| summary.session_id == session_id);

                let Some(summary) = summary else {
                    return Err(AwaitSubagentToolOutput::Error {
                        error: format!("No background sub-agent with session id {session_id}"),
                    });
                };

                if summary.status != "running" {
                    return Ok(AwaitSubagentToolOutput::Finished {
                        session_id,
                        status: summary.status,
                        output: summary.output,
                        error: summary.error,
                    });
                }

                if Instant::now() >= deadline {
                    return Ok(AwaitSubagentToolOutput::TimedOut {
                        session_id,
                        status: summary.status,
                    });
                }

                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
    }
}
