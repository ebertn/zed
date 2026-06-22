use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};

/// Send a follow-up message to a background sub-agent.
///
/// Pass the `session_id` from `spawn_agent_background` (or `list_subagents`).
/// - If the sub-agent is still running, your message is queued and delivered as a
///   follow-up turn once its current turn finishes.
/// - If the sub-agent has finished, it is resumed with your message (it keeps its
///   prior context, so you can send a short, direct instruction).
///
/// This does not wait for the sub-agent to act on the message; use `list_subagents`
/// to check on it afterwards.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct MessageSubagentToolInput {
    /// The session ID of the background sub-agent to message.
    pub session_id: acp::SessionId,
    /// The message to deliver to the sub-agent.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum MessageSubagentToolOutput {
    Delivered {
        session_id: acp::SessionId,
        status: String,
        message: String,
    },
    Error {
        error: String,
    },
}

impl From<MessageSubagentToolOutput> for LanguageModelToolResultContent {
    fn from(output: MessageSubagentToolOutput) -> Self {
        match output {
            MessageSubagentToolOutput::Delivered {
                session_id,
                status,
                message,
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "status": status,
                "message": message,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize message_subagent output: {e}"))
            .into(),
            MessageSubagentToolOutput::Error { error } => {
                serde_json::to_string(&serde_json::json!({ "error": error }))
                    .unwrap_or_else(|e| format!("Failed to serialize message_subagent output: {e}"))
                    .into()
            }
        }
    }
}

/// Tool that delivers a follow-up message to a background sub-agent.
pub struct MessageSubagentTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl MessageSubagentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for MessageSubagentTool {
    type Input = MessageSubagentToolInput;
    type Output = MessageSubagentToolOutput;

    const NAME: &'static str = "message_subagent";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Messaging background sub-agent".into()
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
                .map_err(|e| MessageSubagentToolOutput::Error {
                    error: e.to_string(),
                })?;

            let session_id = input.session_id.clone();
            let result = self
                .environment
                .message_subagent(session_id.clone(), input.message, cx);

            match result {
                Ok(outcome) => {
                    let status = outcome.label().to_string();
                    let message = match outcome {
                        crate::SubagentMessageResult::Queued => format!(
                            "Queued your message for background sub-agent {session_id}; it will \
                             be delivered after its current turn."
                        ),
                        crate::SubagentMessageResult::Resumed => format!(
                            "Resumed background sub-agent {session_id} with your message."
                        ),
                    };
                    Ok(MessageSubagentToolOutput::Delivered {
                        session_id,
                        status,
                        message,
                    })
                }
                Err(error) => Err(MessageSubagentToolOutput::Error {
                    error: error.to_string(),
                }),
            }
        })
    }
}
