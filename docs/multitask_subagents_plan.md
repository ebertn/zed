# Multitask Subagents — Implementation Plan

Branch: `agent-multitask-subagents`
Worktree: `.worktrees/multitask-subagents`
Base: `origin/main`

## Implementation status

- [x] **Phase 1** — background subagent lifecycle: `SubagentStatus`, `BackgroundSubagent`,
  `Thread::background_subagents` registry, and `SubagentHandle::spawn_detached` (driver
  task owned by the parent so it isn't dropped/cancelled).
- [x] **Phase 2** — orchestration tools: `spawn_agent_background`, `list_subagents`
  (monitor + retrieve completed `output`), `message_subagent` (queue a follow-up for a
  running subagent, or resume a finished one), `await_subagent` (blocking join with
  optional timeout), and `cancel_subagent`. All enabled in the `write`/`ask` profiles.
- [x] **Phase 3.3 (auto-pull-when-idle)** — when the primary goes idle with background
  subagents that finished but weren't surfaced, the agent starts a turn delivering their
  results (via the `send_existing` + `handle_thread_events` self-prompt path), then they
  are archived. Triggered by a `BackgroundSubagentsUpdated` event emitted on subagent
  completion and at each turn boundary; gated on idle + no queued user message + not a
  subagent thread, so it never interrupts an active exchange.
- [x] **Phase 6 (cancellation split)** — deterministic lifecycle (option A, matching
  Cursor): background subagents keep running across new user messages and are never
  cancelled by a parent-turn cancel. Root-cause fix: background subagents no longer
  join the turn-scoped `running_subagents` set (driven via `run_prompt(register_running=false)`),
  so the previous race that nondeterministically cancelled one sibling is gone.
  A new turn auto-archives terminal (completed/failed/cancelled) subagents so
  `list_subagents` stays relevant. Explicit `cancel_subagent` /
  `cancel_background_subagent` / `cancel_all_background_subagents` remain for stopping.
- [ ] Remaining Phase 2: none — `await_subagent` and `message_subagent` are done.
- [ ] Phase 3.2 (turn responsiveness during long foreground tools), Phase 7, Phase 8.
- [x] **Phase 7 (settings / gating / limits)**: `background_subagents_enabled`
  (master switch — hides the whole `spawn_agent_background` tool family in
  `enabled_tools` when off) and `max_concurrent_background_subagents` (default 8;
  `spawn_agent_background` returns a clear error once that many are already
  running). Defined in `settings_content`/`agent_settings`/`default.json`.
- [x] **Phase 7 (persistence across restart)**: the background-subagent registry is
  persisted in `DbThread` (session id, label, status, output/error, delivered) and
  restored in `Thread::from_db`. Subagents and their results survive a restart and
  stay listable; ones that were running are restored as **interrupted** (their
  driver can't survive a restart). `BackgroundSubagent.thread`/`driver` are now
  optional to represent restored-but-not-live entries.
  - The **subagent tool-call card is fully restored** after a restart (transcript,
    expand, full-screen). On open, `ConversationView::initial_state` scans the
    reopened thread's tool calls for `subagent_session_info` and calls
    `load_subagent_session` for each, reloading the subagent's `ThreadView` (the
    replay path alone can't — `open_thread` drains replay before the view
    subscribes, so the `SubagentSpawned` event is missed). The spawn tool's
    `replay` also re-emits `subagent_spawned` for the live path.
  - Restored subagents are marked delivered so they never trigger a spurious
    auto-pull "[Automatic update]" after a restart.
  - [ ] Follow-up: nested subagents (a subagent's own subagents) aren't recursively
    reloaded; messaging/resuming a restored subagent across sessions still depends
    on its session being loaded (now true for direct subagents of the root thread).
- [x] **Phase 4 (edit-safety — WriteCoordinator)**: concurrent edits across all
  agents (primary + background subagents) are now serialized per buffer. A
  `WriteCoordinator` app-global maps each buffer's `EntityId` to an
  `async_lock::Mutex`; every `EditSession` (used by both `edit_file` and
  `write_file`) acquires its buffer's lock in `EditSession::new` and holds it for
  the session's lifetime. Same-buffer sessions serialize (the second re-resolves
  against the now-current content and fails loudly via the existing `old_text`
  match if it moved, instead of applying at a stale offset); different-buffer
  sessions run in parallel. This makes it safe for background subagents to edit
  code concurrently with the primary.
- [x] **Phase 5 (partial) — Multitask UI**:
  - Subagent tool-call cards now reflect the **live** background-subagent status
    (spinner while running, check/error/cancelled when terminal) instead of the
    spawn tool's immediate completion. Driven by `background_subagent` status on the
    parent thread; falls back to tool-call status for inline subagents.
  - A persistent **background sub-agents strip** in the activity bar (where "Edits"
    lives), now **collapsible** (Disclosure header with a count + "Cancel All"),
    with **dividers between rows**. Each row shows a live status icon, label, and
    status; **clicking a row opens that sub-agent full-screen** (`navigate_to_thread`);
    running rows have a **red square Stop button** (`IconName::Stop` + `Color::Error`).
    Re-renders via a native-thread observation on `ThreadView` plus
    `AcpThread::refresh_subagent_tool_calls` emitted on `BackgroundSubagentsUpdated`.
  - Note: only **running** sub-agents show a Stop button (you can't stop a finished
    one); finished sub-agents linger with a check until the next turn archives them.
  - [ ] Still to do: Message button in the strip; per-sub-agent token usage.

- [x] **Always messageable (no hard terminal state)** — finished/cancelled subagents
  are NOT deleted from the registry, so they stay listable via `list_subagents` and
  `message_subagent` can always resume them (their session/context still exists).
  This fixes the asymmetry where a user-cancelled subagent became unmessageable
  (the old auto-archive deleted it, so the agent lost the `session_id`). The
  activity-bar strip shows **only running** subagents to avoid clutter; finished
  ones remain reachable through the tools and the transcript cards.

> The earlier "talking to the primary cancels background subagents" limitation is
> resolved by Phase 6. Background subagents are still surfaced through the existing
> subagent tool-call/thread UI; a dedicated Multitask panel (Phase 5) is pending.
> `cancel_all_background_subagents` exists but is not yet wired to parent-thread
> teardown.

Tests (run against the worktree manifest):
```sh
cargo test -p agent --manifest-path .worktrees/multitask-subagents/Cargo.toml subagent
```
- `test_background_subagent_runs_without_blocking_parent`
- `test_parent_turn_cancel_preserves_background_subagent`
- `test_list_subagents_tool_during_turn_does_not_panic`
- `test_new_turn_archives_finished_background_subagents`
- `test_auto_pull_delivers_finished_subagent_when_idle`
- plus all pre-existing subagent tests (27 total, passing).

> Known minor gap: live model/profile/thinking setting changes propagate to
> turn-scoped subagents but not to detached background subagents (they inherit
> settings at spawn time). Wire `background_subagents` into the settings-propagation
> loops if mid-flight propagation is desired.

> Fixed: `list_subagents` initially read the parent thread synchronously inside
> `run`, which double-borrowed the thread (tools run inside the thread's own
> `update`) and panicked with "cannot read Thread while it is already being
> updated". All subagent tools now defer thread access into `cx.spawn`, matching
> the existing tool pattern. The regression test above guards it.



## Goal

Bring a Cursor-style "Multitask" experience to Zed's native agent:

- **Background subagents.** When the primary (foreground) agent delegates work, the
  subagent runs to completion on its own without pinning the primary's turn.
- **Foreground stays live.** The user can keep talking to the primary agent while
  subagents work in the background.
- **Primary can orchestrate.** The primary agent can *monitor* background subagent
  progress, *send messages* to them while they run, and *cancel* them.
- **A Multitask surface.** The UI shows running background subagents with live
  status, and lets the user (and the model) inspect, message, and cancel them.

This plan is written against the code as it exists on this branch. Anchors are
given as `path:symbol` (line numbers are approximate and will drift).

---

## Decisions (locked)

These are settled; the phases below assume them.

1. **Edit safety = serialized apply + verify (correctness first).** All buffer
   mutations from any agent in the tree (primary + every background subagent) go
   through a single `WriteCoordinator`. The coordinator serializes the *apply* step
   and each edit is re-verified against the current buffer state at apply time. If
   the buffer changed such that the edit no longer resolves cleanly, the write
   **fails loudly** (the model gets a clear error and retries) rather than applying
   stale/corrupting changes. Rationale: applying an edit is fast, so serializing it
   costs almost nothing; the slow work (model + tool latency) still runs in
   parallel. This gives true concurrent editing with no silent corruption. (Chosen
   over disjoint-write-set claims, which reject legitimate work the model can't
   predict, and over optimistic+merge, which detects conflicts too late.)
2. **Separate background tool family.** Keep `spawn_agent` (blocking/join) untouched
   and add `spawn_agent_background` + management tools. The output shape differs
   fundamentally between modes (answer vs. handle), so they should not share one tool.
3. **Pull baseline + auto-pull when idle.** The model pulls results via
   `list_subagents`/`await_subagent`. Additionally, whenever the primary agent goes
   idle (turn ended, no queued user message) and there are completed background
   subagents whose results haven't been delivered yet, the primary **automatically**
   collects them and runs a turn to react. It never injects a turn while the user is
   actively conversing — idle is the trigger, so there's no contention for the floor.
4. **Append user input at the next boundary.** Talking to a busy foreground agent
   queues the message and folds it into context at the next step boundary, made
   responsive by adding the queue signal to the tool-wait `select!` (Phase 3.2).

---

## How it works today (baseline)

Subagents are **already independent sessions**, but they are **synchronous to the
model** because the spawning tool call blocks until the subagent's turn finishes.

- `SpawnAgentTool::run` (`crates/agent/src/tools/spawn_agent_tool.rs`) creates a real
  `Entity<Thread>` + `AcpThread` via `NativeThreadEnvironment::create_subagent_thread`
  (`crates/agent/src/agent.rs`) and then **awaits** `subagent.send(message).await`.
- The parent turn collects each tool's task into a
  `FuturesUnordered<Task<LanguageModelToolResult>>` and drains it before the model
  can continue:
  `while let Some(tool_result) = tool_results.next().await` in
  `Thread::run_turn_internal` (`crates/agent/src/thread.rs`).
- The tool-call contract requires a result before the model gets another turn, so a
  blocking subagent freezes the primary.

Partial infrastructure we will build on:

- `Thread::running_subagents: Vec<WeakEntity<Thread>>` — tracked live subagents;
  settings propagation and cascading cancel already use it.
- `Thread::register_running_subagent` / `unregister_running_subagent`.
- `Thread::has_queued_message` + `AgentSettings::interrupt_turn_for_queued_message`
  — the turn ends at a message boundary when a queued message exists
  (`thread.rs` boundary check near the end of `run_turn_internal`).
- `AcpThreadEvent::SubagentSpawned`, `AcpThread::subagent_spawned`,
  `AcpThread::tool_call_for_subagent`, `ToolCall::is_subagent`,
  `subagent_session_info` meta — subagent sessions already render as their own
  threads/tool calls in the UI.
- `NativeThreadEnvironment::resume_subagent_thread` — follow-up messages to an
  existing subagent session (today only reachable sequentially via `spawn_agent`
  with a `session_id`).

The work below is fundamentally about **lifetime ownership** (detached tasks must
not be dropped/cancelled), **result re-entry** (results arrive in a later turn, not
through the original `tool_use`), **cancellation policy**, **concurrent buffer
writes**, and **UI/orchestration surfaces**.

---

## Phasing overview

| Phase | Theme | Risk |
|------|-------|------|
| 1 | Background subagent lifecycle (detach + status registry) | Med |
| 2 | Orchestration tools for the primary agent | Low |
| 3 | Non-blocking turn + foreground interactivity | Med |
| 4 | Concurrent edit safety (action log / buffers) | **High** |
| 5 | Multitask UI (monitor / message / cancel) | Med |
| 6 | Cancellation & lifecycle semantics | Med |
| 7 | Persistence, settings, telemetry | Low |
| 8 | Tests | Med |

Phases 1–3 deliver a usable end-to-end vertical slice. Phase 4 is the hardest and
gates the primary agent *editing files* concurrently with subagents.

---

## Phase 1 — Background subagent lifecycle

Decouple a subagent's lifetime from the tool call that spawned it.

### 1.1 Status model

Add a status type and a registry on the parent `Thread`.

```rust
// crates/agent/src/thread.rs
pub enum SubagentStatus {
    Running,
    AwaitingInput,          // optional, if we surface permission prompts
    Completed { output: String },
    Failed { error: String },
    Cancelled,
}

pub struct BackgroundSubagent {
    pub session_id: acp::SessionId,
    pub label: SharedString,
    pub status: SubagentStatus,
    pub thread: WeakEntity<Thread>,
    /// The detached driver task. Storing it here keeps it alive; dropping it
    /// cancels the subagent (GPUI Task semantics).
    _task: Task<()>,
}
```

Add to `Thread`:

```rust
background_subagents: HashMap<acp::SessionId, BackgroundSubagent>,
```

> Trap: today the only thing keeping a subagent alive is the parent awaiting the
> `send` task inside `SpawnAgentTool::run`. A detached subagent **must** have its
> driver `Task` stored in `background_subagents`, or GPUI will cancel it.

### 1.2 Detached spawn path

In `NativeSubagentHandle` (`crates/agent/src/agent.rs`), the `send` task already:
- registers via `register_running_subagent`,
- drives the `AcpThread::send` to completion,
- unregisters on completion.

Add a `send_detached` (or a flag) that wraps that same future but, on completion,
writes the terminal `SubagentStatus` into the parent's `background_subagents` map
and emits an event (Phase 5) instead of returning the value to a blocked caller.

New method on the `SubagentHandle` trait (`crates/agent/src/thread.rs`):

```rust
fn spawn_detached(
    &self,
    message: String,
    cx: &mut AsyncApp,
) -> acp::SessionId; // returns immediately; drives in background
```

`NativeThreadEnvironment::create_subagent`/`resume_subagent` already return a
`SubagentHandle`; the detached driver is created in the tool (Phase 2) and stored on
the parent thread.

### 1.3 Progress signal

Background subagents already emit `AcpThreadEvent`s on their own `AcpThread`. The
parent subscribes (Phase 5) to keep `BackgroundSubagent.status`/progress fresh. No
new protocol is required for monitoring — reuse the per-subagent `AcpThread`.

---

## Phase 2 — Orchestration tools for the primary agent

Give the model verbs to fan out and manage background work. Register them in
`Thread::add_default_tools` (`crates/agent/src/thread.rs`) under the existing
`depth() < MAX_SUBAGENT_DEPTH` guard, next to `SpawnAgentTool`. New files under
`crates/agent/src/tools/`, exported from `crates/agent/src/tools.rs`.

Decision: keep the existing `spawn_agent` (blocking/join) semantics, and add a
parallel **background** family. This avoids breaking current callers/prompts.

1. **`spawn_agent_background`** — start a subagent and return immediately with
   `{ session_id, label }`. Stores the detached driver in `background_subagents`.
   - Reuses `ThreadEnvironment::create_subagent` + the Phase 1 detached driver.
2. **`list_subagents`** — return `[{ session_id, label, status, last_activity,
   summary }]` from `background_subagents`. The model's "monitor" verb.
3. **`get_subagent_output`** / **`await_subagent`** — two modes:
   - non-blocking: return current status + latest assistant message;
   - blocking (`wait: true`): await the stored driver task and return final output
     (the "join" path — composes with the current turn loop).
4. **`message_subagent`** — send a follow-up to a *running or idle* background
   subagent (`resume_subagent_thread` + enqueue on that subagent's thread). If the
   subagent is mid-turn, this uses the subagent's own queued-message machinery.
5. **`cancel_subagent`** — cancel one background subagent (Phase 6). Returns final
   partial status.

Tool I/O mirrors `SpawnAgentTool` (typed `Input`/`Output`, `LanguageModelToolResultContent`).
Attach `subagent_session_info` meta so each maps onto the existing UI plumbing.

> Result re-entry: because `spawn_agent_background` returns immediately, the
> subagent's *final* output cannot flow back through the original `tool_use`. The
> model retrieves it later via `get_subagent_output`/`await_subagent`, or it is
> pushed (Phase 3.3).

---

## Phase 3 — Non-blocking turn + foreground interactivity

### 3.1 Don't block the primary turn on background subagents

`spawn_agent_background::run` returns a ready result immediately; it never awaits the
subagent. The driver lives in `background_subagents`. The primary's
`tool_results.next().await` drain no longer includes background subagents, so the
primary keeps going / ends its turn normally.

### 3.2 Make the foreground responsive while subagents run

Today queued user messages are only honored at the model-request boundary
(`interrupt_turn_for_queued_message` check at the end of `run_turn_internal`). With
background subagents the primary may be idle (turn ended) — in that case the user
simply sends normally. The remaining gap is long-running *foreground* tool calls; to
keep the UX consistent, add `has_queued_message` (as a `watch` channel) into the
`futures::select!` at the top of the tool loop in `run_turn_internal`
(`crates/agent/src/thread.rs`) so a new user message can end the current step
promptly. Background subagents keep running because they are no longer owned by the
turn.

Replace the bare `has_queued_message: bool` with a `watch::Sender<bool>` (or deliver
the actual queued content over a channel) so the running turn can observe changes
without polling. UI already drives this via
`ThreadView::sync_queue_flag_to_native_thread`.

### 3.3 Auto-pull completed subagents when the primary is idle

Decision #3: the primary pulls results explicitly via the tools, **and** also
collects finished work automatically when it has nothing else to do.

Track which completed background subagents have not yet had their results delivered
to the primary (a `delivered: bool` on `BackgroundSubagent`, or a
`pending_delivery: VecDeque<SessionId>`). When the primary becomes idle:

- detect idle at the point the running turn finishes in `run_turn` (the
  `running_turn.take()` cleanup) / when `status()` returns to `Idle`;
- if there is **no** queued/in-flight user message **and** there are undelivered
  completed subagents, enqueue a synthetic summary message ("Background subagent
  `<label>` finished: <output>") and start a turn via `Thread::send_existing`.

Guards:
- Never fire while a turn is running or while `has_queued_message` is set — the user
  always has the floor; idle is the only trigger, so there is no contention.
- Debounce/batch: if several subagents finish close together, deliver them in one
  turn rather than one turn each (drain `pending_delivery` together).
- Mark each as delivered before starting the turn so a failed/cancelled auto-turn
  doesn't loop. Cancelled/failed subagents are also "completed" for delivery so the
  model learns they stopped.

This is gentler than push-during-conversation: completions surface promptly when the
user steps away, but never interrupt an active exchange.

---

## Phase 4 — Concurrent edit safety (serialized apply + verify)

This gates allowing the **primary agent to edit files while subagents also edit**.
Decision #1: correctness over parallelism, via a single `WriteCoordinator`.

Subagents share buffers with the parent through linked action logs
(`crates/action_log/src/action_log.rs`: `linked_action_log` — "track individual
diffs for this subagent, but also associate the reads/writes with a parent review
experience"). Today only one agent writes at a time, so this is safe. Background
subagents + a concurrently-editing primary means **multiple writers to the same
buffers simultaneously**.

### Approach

Introduce a `WriteCoordinator`, shared across the whole agent tree (created on the
root `Thread`, handed to each subagent's `ThreadEnvironment` / `ActionLog` so they
all reference the same instance). Every buffer-mutating tool acquires the
coordinator before applying and follows **apply-time verification**:

1. **Resolve** the edit against a snapshot of the buffer (as today).
2. **Acquire** the coordinator's write guard. Because applying an edit is fast
   (milliseconds) while model/tool latency dominates, a single global async mutex
   (`gpui`/`futures` `Mutex`) over the apply step is sufficient and simplest; the
   parallel work — thinking, searching, reading — is unaffected. (Can be sharded
   per-buffer later if profiling ever shows apply-step contention, but it won't be
   the bottleneck.)
3. **Verify** under the guard that the buffer still matches what the edit was
   resolved against (anchor/old-text still present, expected version unchanged).
4. **Apply** if verification passes; **fail loudly** otherwise with an error that
   tells the model the file changed underneath it and to re-read and retry. Never
   apply a stale edit.
5. **Release** the guard.

This yields true concurrent editing (different files proceed freely; same-file edits
serialize and re-verify) with **no silent corruption** — the worst case is a clean,
recoverable "edit no longer applies" error.

### Touch points

- New `WriteCoordinator` (likely in `crates/action_log` or `crates/agent`), holding
  the async write mutex and, if needed later, per-buffer version tracking.
- Thread the coordinator from the root `Thread` to subagents at creation
  (`Thread::new_subagent` / `NativeThreadEnvironment`) so all share one instance.
- `EditFileTool` (`crates/agent/src/tools/edit_file_tool.rs`) is the primary site:
  wrap resolve→verify→apply in the guard. Confirm whether its current fuzzy
  `old_text` resolution already fails when text is absent — if so, verification is
  mostly "re-resolve under the lock," which is a small change.
- `WriteFileTool`, `CreateDirectoryTool`, `MovePathTool`, `DeletePathTool`,
  `CopyPathTool` — acquire the guard around their mutation; for whole-file/path ops
  verification is existence/identity checks.
- `ActionLog` linked-log accounting stays as-is for review aggregation.

### Sequencing

Land the coordinator and route the primary + subagent edit tools through it **before**
enabling background subagents to write. Until then, gate background subagents to
read-only/non-edit tool profiles so the unsafe window never opens in shipped builds.

---

## Phase 5 — Multitask UI

A surface to monitor, message, and cancel background subagents — the user-facing
half of "Multitask".

- The parent `ThreadView` subscribes to each background subagent's `AcpThread`
  events (reuse `AcpThreadEvent`) and to a new parent-thread event when
  `background_subagents` changes.
- Add a **Multitask panel / activity strip** in `crates/agent_ui` showing each
  background subagent: label, status, latest activity line, token usage, and
  controls: **Open** (focus the subagent `ThreadView`), **Message**, **Cancel**.
  - Reuse `render_subagent_tool_call` / `render_subagent_titlebar` /
    `is_subagent` plumbing in `crates/agent_ui/src/conversation_view/thread_view.rs`.
  - The existing activity bar (`render_activity_bar`) and queued-message UI are good
    models for the layout.
- "Message" routes to the `message_subagent` path (Phase 2.4) via the subagent's own
  `ThreadView`/`MessageEditor` (subagents currently hide their editor — re-enable a
  constrained editor for background subagents, or route through the panel).
- Distinguish background subagents (detached) from legacy inline subagents in the
  tool-call rendering.

### Concrete feedback to address (from testing)

1. **Live status on the spawn tool-call block.** The `spawn_agent_background`
   tool-call block always renders a green check because the *tool* completes
   immediately (that's what makes it non-blocking). It should instead reflect the
   *subagent session's* live status: show a spinner while the subagent is
   `running`, and a check/error icon when it reaches a terminal state. Drive the
   icon/label from `background_subagents[session_id].status` (or the subagent's
   `AcpThread` status) rather than the tool call's status. The subagent
   `session_id` is already on the tool call via `subagent_session_info`.
2. **Persistent "what subagents exist" strip.** Surface the live list of background
   subagents (label + status, with Open/Message/Cancel) in the activity-bar region
   between the agent output and the message editor — i.e. alongside where "Edits"
   renders (`render_activity_bar` in `thread_view.rs`), so it's always visible while
   they run, not buried in the transcript.

---

## Phase 6 — Cancellation & lifecycle semantics

Today `Thread::cancel` drains and cancels `running_subagents` — it assumes subagents
are owned by the current turn. Background subagents outlive the turn, so:

- Split "cancel this turn" from "cancel background work."
  - Ending/cancelling a primary turn must **not** kill detached
    `background_subagents`.
  - Add `cancel_subagent(session_id)` and `cancel_all_subagents()` for explicit
    control (used by the tool in Phase 2.5 and the UI in Phase 5).
- Define cleanup: on subagent completion/failure/cancel, set terminal status, keep
  the entry visible for inspection, and drop the driver task. Decide retention
  (keep until parent thread closes? until user dismisses?).
- Closing/deleting the parent thread cancels all its background subagents.
- Respect `MAX_SUBAGENT_DEPTH`; background subagents spawning their own subagents
  inherit the same depth rules.

---

## Phase 7 — Persistence, settings, telemetry

- **Settings** (`crates/agent_settings/src/agent_settings.rs`): add e.g.
  `background_subagents_enabled`, `max_concurrent_background_subagents`,
  `push_subagent_completions` (Phase 3.3), and reuse/extend
  `interrupt_turn_for_queued_message`. Mirror the `from_settings` wiring and the
  test struct in `crates/agent_ui` that constructs `AgentSettings`.
- **Persistence**: background subagents are sessions (already persisted via
  `register_session`). Persist enough of `background_subagents` (status, label,
  links) to restore the Multitask panel after reload, or document that in-flight
  background work does not survive restart in v1.
- **Telemetry**: extend existing `"Subagent Started"` / `"Subagent Completed"`
  events (`crates/agent/src/agent.rs`) with `mode = "background"`, concurrency count,
  cancel reason.

---

## Phase 8 — Tests

Follow the existing GPUI test patterns in `crates/agent/src/tests/mod.rs`
(see `test_parent_cancel_stops_subagent`, `test_queued_message_ends_turn_at_boundary`,
`FakeThreadEnvironment` / `FakeSubagentHandle`). Per `.agents/skills/gpui-test`,
prefer GPUI executor timers and `run_until_parked`.

Coverage:
1. `spawn_agent_background` returns immediately; primary turn ends without waiting.
2. Detached driver survives (not dropped); subagent runs to completion and status
   becomes `Completed`.
3. `list_subagents` reflects running → completed transitions.
4. `await_subagent`/`get_subagent_output` returns final output after completion and
   current state while running.
5. `message_subagent` delivers a follow-up to a running background subagent.
6. `cancel_subagent` stops one subagent without affecting others or the primary.
7. User message during a long foreground tool ends the step promptly (Phase 3.2)
   while background subagents keep running.
8. Phase 4: concurrent writes to the same path are rejected/serialized; disjoint
   paths proceed in parallel.
9. Cancelling the primary turn does **not** cancel background subagents; closing the
   parent thread does.

`FakeSubagentHandle::num_entries` currently `unimplemented!()` — implement it for the
new tests.

---

## File-by-file change map (first pass)

- `crates/agent/src/thread.rs`
  - `BackgroundSubagent`, `SubagentStatus`, `background_subagents` map.
  - `has_queued_message` → `watch` channel; `select!` in `run_turn_internal`.
  - Split cancel semantics; `cancel_subagent`/`cancel_all_subagents`.
  - Register background tools in `add_default_tools`.
- `crates/agent/src/agent.rs`
  - `NativeSubagentHandle` detached driver writing terminal status back to parent.
  - `spawn_detached` on the `SubagentHandle` trait + impl.
  - Telemetry `mode = "background"`.
- `crates/agent/src/tools/spawn_agent_background_tool.rs` (new)
- `crates/agent/src/tools/list_subagents_tool.rs` (new)
- `crates/agent/src/tools/subagent_output_tool.rs` (new)
- `crates/agent/src/tools/message_subagent_tool.rs` (new)
- `crates/agent/src/tools/cancel_subagent_tool.rs` (new)
- `crates/agent/src/tools.rs` — module decls + re-exports.
- `crates/action_log/src/action_log.rs` — `WriteCoordinator` (Phase 4).
- `crates/agent/src/tools/edit_file_tool.rs` (+ write/move/delete/create tools) —
  consult coordinator.
- `crates/agent_ui/src/conversation_view/thread_view.rs` — Multitask panel,
  message/cancel controls, background-subagent rendering.
- `crates/agent_settings/src/agent_settings.rs` — new settings + `from_settings`.
- `crates/agent/src/tests/mod.rs` — tests; implement `FakeSubagentHandle::num_entries`.
- `assets/settings/default.json` — enable new tools in `write`/`ask` profiles.

---

## Open questions

Resolved (see Decisions above):
- ~~Append vs. interrupt~~ → **append at boundary** (#4).
- ~~Edit-safety strategy~~ → **serialized apply + verify via `WriteCoordinator`** (#1).
- ~~Push vs. pull~~ → **pull + auto-pull when idle** (#3).
- ~~Separate tool vs. flag~~ → **separate `spawn_agent_background` family** (#2).

Still open:
1. **Retention/persistence** of background subagents across restart: restore the
   Multitask panel from persisted sessions, or document that in-flight background
   work does not survive a reload in v1?
2. **Concurrency cap**: default value for `max_concurrent_background_subagents`, and
   behavior on overflow (reject the spawn vs. queue it).
3. **Auto-pull batching window**: how long to debounce near-simultaneous completions
   before delivering them in one idle turn.
4. **Edit-tool verification depth**: does `EditFileTool`'s existing resolution
   already fail cleanly on absent `old_text` (making verification cheap), or do we
   need explicit buffer-version checks? Confirm while implementing Phase 4.

---

## Suggested commit sequence

1. `agent: Add background subagent status model and registry` (Phase 1).
2. `agent: Add detached subagent driver` (Phase 1).
3. `agent: Add spawn_agent_background and list_subagents tools` (Phase 2).
4. `agent: Add await/message/cancel subagent tools` (Phase 2).
5. `agent: Keep primary turn responsive with background subagents` (Phase 3).
6. `action_log: Coordinate concurrent agent writes` (Phase 4).
7. `agent_ui: Add Multitask panel for background subagents` (Phase 5).
8. `agent: Background subagent cancellation and lifecycle` (Phase 6).
9. `agent: Settings, persistence, telemetry for Multitask` (Phase 7).
10. `agent: Tests for background subagents` (Phase 8).

Each PR should follow repo PR hygiene (imperative title, no conventional-commit
prefix, `Release Notes:` section).
```