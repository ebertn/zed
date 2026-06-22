use acp_thread::{SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo};
use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};
use agent_settings::AgentSettings;
use settings::Settings as _;

/// Spawn a sub-agent that runs in the background.
///
/// Unlike `spawn_agent`, this returns immediately with a `session_id` instead of
/// waiting for the sub-agent to finish. The sub-agent keeps working while you
/// continue with other work or talk to the user.
///
/// ### When to use this
/// - You want to fan out one or more long-running, well-scoped subtasks and keep
///   making progress (or stay responsive to the user) instead of blocking.
/// - The subtasks are independent and can run concurrently. For code-edit subtasks,
///   give each sub-agent a disjoint set of files to avoid conflicts.
///
/// ### Designing delegated subtasks
/// - A sub-agent does not see your conversation history. Include all relevant context
///   (file paths, requirements, constraints) in the message.
/// - Subtasks must be concrete, well-defined, and self-contained.
///
/// ### Getting results back
/// - This tool does NOT return the sub-agent's answer. It returns a `session_id`.
/// - Results are NOT delivered to you automatically. To act on a sub-agent's
///   work you must explicitly check on it: use `list_subagents` to monitor
///   progress and read completed output, or `await_subagent` to block until a
///   specific sub-agent finishes and return its result.
/// - Staying responsive to the user does not require collecting results
///   immediately: you can spawn, keep talking to the user, and collect later.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SpawnAgentBackgroundToolInput {
    /// Short label displayed in the UI while the agent runs (e.g., "Researching alternatives")
    pub label: String,
    /// The prompt for the agent. Include full context needed for the task. For
    /// follow-ups (with session_id), you can rely on the agent already having the
    /// previous message.
    pub message: String,
    /// Session ID of an existing agent session to continue in the background
    /// instead of creating a new one.
    #[serde(default)]
    pub session_id: Option<acp::SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum SpawnAgentBackgroundToolOutput {
    Started {
        session_id: acp::SessionId,
        status: String,
        message: String,
        session_info: SubagentSessionInfo,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        session_id: Option<acp::SessionId>,
        error: String,
    },
}

impl From<SpawnAgentBackgroundToolOutput> for LanguageModelToolResultContent {
    fn from(output: SpawnAgentBackgroundToolOutput) -> Self {
        match output {
            SpawnAgentBackgroundToolOutput::Started {
                session_id,
                status,
                message,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "status": status,
                "message": message,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize spawn_agent_background output: {e}"))
            .into(),
            SpawnAgentBackgroundToolOutput::Error { session_id, error } => {
                serde_json::to_string(&serde_json::json!({
                    "session_id": session_id,
                    "error": error,
                }))
                .unwrap_or_else(|e| {
                    format!("Failed to serialize spawn_agent_background output: {e}")
                })
                .into()
            }
        }
    }
}

/// Tool that spawns a sub-agent thread to work on a task in the background.
pub struct SpawnAgentBackgroundTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl SpawnAgentBackgroundTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for SpawnAgentBackgroundTool {
    type Input = SpawnAgentBackgroundToolInput;
    type Output = SpawnAgentBackgroundToolOutput;

    const NAME: &'static str = "spawn_agent_background";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => i.label.into(),
            Err(value) => value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| SharedString::from(s.to_owned()))
                .unwrap_or_else(|| "Spawning background agent".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| SpawnAgentBackgroundToolOutput::Error {
                    session_id: None,
                    error: e.to_string(),
                })?;

            let label = input.label.clone();

            // Enforce the concurrency cap so a runaway agent can't spawn an
            // unbounded number of background sub-agents.
            let (max_concurrent, running) = cx.update(|cx| {
                let max = AgentSettings::get_global(cx).max_concurrent_background_subagents;
                let running = self
                    .environment
                    .list_subagents(cx)
                    .into_iter()
                    .filter(|subagent| subagent.status == "running")
                    .count();
                (max, running)
            });
            if max_concurrent > 0 && running >= max_concurrent {
                return Err(SpawnAgentBackgroundToolOutput::Error {
                    session_id: None,
                    error: format!(
                        "Cannot start another background sub-agent: {running} are already \
                         running (limit {max_concurrent}). Wait for one to finish or cancel \
                         one with cancel_subagent."
                    ),
                });
            }

            let (subagent, session_info) = cx.update(|cx| {
                let subagent = if let Some(session_id) = input.session_id.clone() {
                    self.environment.resume_subagent(session_id, cx)
                } else {
                    self.environment.create_subagent(label.clone(), cx)
                };
                let subagent = subagent.map_err(|err| SpawnAgentBackgroundToolOutput::Error {
                    session_id: None,
                    error: err.to_string(),
                })?;
                let session_info = SubagentSessionInfo {
                    session_id: subagent.id(),
                    message_start_index: subagent.num_entries(cx),
                    message_end_index: None,
                };

                event_stream.subagent_spawned(subagent.id());
                event_stream.update_fields_with_meta(
                    acp::ToolCallUpdateFields::new(),
                    Some(acp::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        serde_json::json!(&session_info),
                    )])),
                );

                Ok((subagent, session_info))
            })?;

            let session_id = session_info.session_id.clone();

            if let Err(error) = subagent.spawn_detached(input.message, label.into(), cx) {
                return Err(SpawnAgentBackgroundToolOutput::Error {
                    session_id: Some(session_id),
                    error: error.to_string(),
                });
            }

            telemetry::event!(
                "Subagent Started",
                subagent_session = session_id.to_string(),
                mode = "background",
            );

            let note = format!(
                "Started background sub-agent (session {session_id}). It is running now; \
                 use list_subagents to monitor it and collect its result later."
            );

            event_stream.update_fields_with_meta(
                acp::ToolCallUpdateFields::new().content(vec![note.clone().into()]),
                Some(acp::Meta::from_iter([(
                    SUBAGENT_SESSION_INFO_META_KEY.into(),
                    serde_json::json!(&session_info),
                )])),
            );

            Ok(SpawnAgentBackgroundToolOutput::Started {
                session_id,
                status: "running".to_string(),
                message: note,
                session_info,
            })
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        let (content, session_info) = match output {
            SpawnAgentBackgroundToolOutput::Started {
                message,
                session_info,
                ..
            } => (message, Some(session_info)),
            SpawnAgentBackgroundToolOutput::Error { error, .. } => (error, None),
        };

        // Re-announce the subagent so its session is reloaded after a restart,
        // restoring the tool-call card's transcript / expand / full-screen.
        if let Some(session_info) = &session_info {
            event_stream.subagent_spawned(session_info.session_id.clone());
        }

        let meta = session_info.map(|session_info| {
            acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )])
        });
        event_stream.update_fields_with_meta(
            acp::ToolCallUpdateFields::new().content(vec![content.into()]),
            meta,
        );

        Ok(())
    }
}
