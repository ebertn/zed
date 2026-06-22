use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};

/// Cancel a running background sub-agent that you started with
/// `spawn_agent_background`.
///
/// Pass the `session_id` returned by `spawn_agent_background` (or reported by
/// `list_subagents`). The sub-agent's in-flight work is stopped and its status
/// becomes `cancelled`. Cancelling an already-finished sub-agent is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct CancelSubagentToolInput {
    /// The session ID of the background sub-agent to cancel.
    pub session_id: acp::SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum CancelSubagentToolOutput {
    Cancelled {
        session_id: acp::SessionId,
        status: String,
        message: String,
    },
    Error {
        error: String,
    },
}

impl From<CancelSubagentToolOutput> for LanguageModelToolResultContent {
    fn from(output: CancelSubagentToolOutput) -> Self {
        match output {
            CancelSubagentToolOutput::Cancelled {
                session_id,
                status,
                message,
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "status": status,
                "message": message,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize cancel_subagent output: {e}"))
            .into(),
            CancelSubagentToolOutput::Error { error } => {
                serde_json::to_string(&serde_json::json!({ "error": error }))
                    .unwrap_or_else(|e| format!("Failed to serialize cancel_subagent output: {e}"))
                    .into()
            }
        }
    }
}

/// Tool that cancels a background sub-agent.
pub struct CancelSubagentTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl CancelSubagentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for CancelSubagentTool {
    type Input = CancelSubagentToolInput;
    type Output = CancelSubagentToolOutput;

    const NAME: &'static str = "cancel_subagent";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Cancelling background sub-agent".into()
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
                .map_err(|e| CancelSubagentToolOutput::Error {
                    error: e.to_string(),
                })?;

            let session_id = input.session_id;
            let result = cx.update(|cx| self.environment.cancel_subagent(session_id.clone(), cx));

            match result {
                Ok(summary) => Ok(CancelSubagentToolOutput::Cancelled {
                    session_id: summary.session_id,
                    status: summary.status,
                    message: format!("Cancelled background sub-agent {session_id}."),
                }),
                Err(error) => Err(CancelSubagentToolOutput::Error {
                    error: error.to_string(),
                }),
            }
        })
    }
}
