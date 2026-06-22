use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, SubagentSummary, ThreadEnvironment, ToolCallEventStream, ToolInput};

/// List the background sub-agents you have spawned, with their current status.
///
/// Use this to monitor sub-agents started with `spawn_agent_background`. Each
/// entry reports its `session_id`, `label`, and `status` (one of `running`,
/// `completed`, `failed`, `cancelled`). Completed sub-agents include their final
/// `output`; failed ones include an `error`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ListSubagentsToolInput {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSubagentsToolOutput {
    pub subagents: Vec<SubagentSummary>,
}

impl From<ListSubagentsToolOutput> for LanguageModelToolResultContent {
    fn from(output: ListSubagentsToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize list_subagents output: {e}"))
            .into()
    }
}

/// Tool that lists the parent thread's background sub-agents.
pub struct ListSubagentsTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl ListSubagentsTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for ListSubagentsTool {
    type Input = ListSubagentsToolInput;
    type Output = ListSubagentsToolOutput;

    const NAME: &'static str = "list_subagents";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Listing background sub-agents".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        // Defer reading the parent thread until after the current turn update
        // finishes. Tools run inside the thread's own `update`, so reading the
        // thread synchronously here would double-borrow it and panic.
        cx.spawn(async move |cx| {
            let subagents = cx.update(|cx| self.environment.list_subagents(cx));
            Ok(ListSubagentsToolOutput { subagents })
        })
    }
}
