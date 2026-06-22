//! Canvas surface: a native WebView embedded in the editor pane, exposed via the
//! `surface` registry (so a user action or an agent tool can open one) and three
//! `agent` tools (`canvas_open`, `canvas_update`, `canvas_list`) so the agent can
//! create, drive, and reason about canvases.
//!
//! Each canvas is a workspace `Item` (tab). The web content is a `WKWebView` (via
//! `wry`) added as a sibling of GPUI's Metal view under the window's
//! `contentView`, repositioned every frame to track the tab's on-screen bounds
//! (reported by a `canvas()` element). Parenting to `contentView` (not GPUI's
//! Metal `native_view`) is what keeps it stable: GPUI's per-frame
//! `invalidateCursorRectsForView:` and the display-link `step` only traverse the
//! Metal view's subtree, so a sibling WebView never trips either path.

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Global, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, Task, WeakEntity, Window, WindowBackgroundAppearance,
    actions, canvas, div,
};
use workspace::{Item, Workspace};

use agent::{AgentTool, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use editor::Editor;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use surface::{SurfaceId, SurfaceProvider};
use ui::{LabelSize, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple};

#[cfg(target_os = "macos")]
use std::{cell::RefCell, rc::Rc};

actions!(
    canvas,
    [
        /// Opens a "Canvas" tab backed by a native WebView in the active pane.
        OpenCanvasSpike,
        /// Brings the next open canvas to the foreground (cycles through them).
        FocusNextCanvas,
    ]
);

pub fn init(cx: &mut App) {
    surface::register_surface_provider(cx, Arc::new(CanvasProvider));
    surface::load_persisted(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &OpenCanvasSpike, window, cx| {
            let path = match prepare_canvas_file("Canvas Spike", DEFAULT_CANVAS) {
                Ok(path) => path,
                Err(err) => {
                    log::error!("canvas: {err:#}");
                    return;
                }
            };
            let params =
                serde_json::json!({ "title": "Canvas Spike", "path": path.to_string_lossy() });
            if let Err(err) = surface::open_surface("canvas", params, workspace, window, cx) {
                log::error!("canvas: open_surface failed: {err:#}");
            }
            workspace
                .project()
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(canvases_dir(), false, cx)
                })
                .detach();
        });

        workspace.register_action(|workspace, _: &FocusNextCanvas, window, cx| {
            focus_next_canvas(workspace, window, cx);
        });

        // Remember this workspace + its window so agent tools (which only get an
        // `&mut App`) can open canvases into the workspace the user is in. For a
        // single-window setup this is exactly "the active workspace".
        if let Some(window) = window {
            set_active_workspace(cx.weak_entity(), window.window_handle(), cx);
        }
    })
    .detach();

    // Expose the canvas tools on every thread. Note: a tool is only shown to the
    // model if the active agent profile lists it (`agent.profiles.*.tools` in
    // settings) — these three are enabled in the default `write`/`ask` profiles.
    cx.observe_new(|thread: &mut Thread, _window, _cx| {
        thread.add_tool(CanvasOpenTool {
            session_id: thread.id().clone(),
        });
        thread.add_tool(CanvasListTool);
        thread.add_tool(CanvasErrorsTool);
    })
    .detach();
}

// --- Active-workspace tracking (so agent tools can find a workspace) ----------

#[derive(Default)]
struct ActiveWorkspace(Option<(WeakEntity<Workspace>, AnyWindowHandle)>);

impl Global for ActiveWorkspace {}

fn set_active_workspace(workspace: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    if !cx.has_global::<ActiveWorkspace>() {
        cx.set_global(ActiveWorkspace::default());
    }
    cx.global_mut::<ActiveWorkspace>().0 = Some((workspace, window));
}

fn active_workspace(cx: &App) -> Option<(WeakEntity<Workspace>, AnyWindowHandle)> {
    cx.try_global::<ActiveWorkspace>()
        .and_then(|active| active.0.clone())
}

// --- Canvas instance registry (id -> view) -----------------------------------

#[derive(Default)]
struct CanvasRegistry {
    next_id: u64,
    last_focused: Option<u64>,
    instances: HashMap<u64, WeakEntity<CanvasView>>,
    /// Release observers (keyed by canvas id) that deregister a canvas and
    /// recompute its window's transparency when its tab is closed.
    release_subscriptions: HashMap<u64, Subscription>,
}

impl Global for CanvasRegistry {}

fn register_canvas(view: &Entity<CanvasView>, window: AnyWindowHandle, cx: &mut App) -> u64 {
    if !cx.has_global::<CanvasRegistry>() {
        cx.set_global(CanvasRegistry::default());
    }
    let id = {
        let registry = cx.global_mut::<CanvasRegistry>();
        let id = registry.next_id;
        registry.next_id += 1;
        registry.instances.insert(id, view.downgrade());
        id
    };

    // When the canvas tab is closed (the view is released), drop it from the
    // registry and restore the window to opaque if it was the last canvas there.
    let subscription = cx.observe_release(view, move |_view, cx| {
        if let Some(registry) = try_global_mut::<CanvasRegistry>(cx) {
            registry.instances.remove(&id);
            registry.release_subscriptions.remove(&id);
            if registry.last_focused == Some(id) {
                registry.last_focused = None;
            }
        }
        refresh_window_transparency(window, cx);
    });
    cx.global_mut::<CanvasRegistry>()
        .release_subscriptions
        .insert(id, subscription);

    refresh_window_transparency(window, cx);
    id
}

fn canvas_by_id(id: u64, cx: &App) -> Option<Entity<CanvasView>> {
    cx.try_global::<CanvasRegistry>()?
        .instances
        .get(&id)?
        .upgrade()
}

/// Currently-open canvases as `(id, title)`, sorted by id. Dead entries (closed
/// tabs) are skipped.
fn live_canvases(cx: &App) -> Vec<(u64, SharedString)> {
    let Some(registry) = cx.try_global::<CanvasRegistry>() else {
        return Vec::new();
    };
    let mut canvases: Vec<(u64, SharedString)> = registry
        .instances
        .iter()
        .filter_map(|(id, weak)| weak.upgrade().map(|view| (*id, view.read(cx).title.clone())))
        .collect();
    canvases.sort_by_key(|(id, _)| *id);
    canvases
}

/// Whether `window` currently hosts at least one live canvas.
fn window_has_live_canvas(window: AnyWindowHandle, cx: &App) -> bool {
    let Some(registry) = cx.try_global::<CanvasRegistry>() else {
        return false;
    };
    registry.instances.values().any(|weak| {
        weak.upgrade()
            .is_some_and(|view| view.read(cx).window_handle == window)
    })
}

/// Makes `window`'s surface non-opaque while it hosts a canvas (so the WebView
/// shows through the punched transparency hole) and restores it to opaque once
/// the last canvas closes (to regain the direct-to-display fast path).
fn refresh_window_transparency(window: AnyWindowHandle, cx: &mut App) {
    let appearance = if window_has_live_canvas(window, cx) {
        WindowBackgroundAppearance::Transparent
    } else {
        WindowBackgroundAppearance::Opaque
    };
    if let Err(err) = window.update(cx, |_root, window, _cx| {
        window.set_background_appearance(appearance);
    }) {
        log::error!("canvas: failed to update window transparency: {err}");
    }
}

// --- Surface provider --------------------------------------------------------

/// Opens/updates the WebView canvas. Routed through the `surface` registry so the
/// user action and the agent tools share one path.
struct CanvasProvider;

#[derive(Default, Deserialize)]
struct CanvasParams {
    title: Option<String>,
    path: Option<String>,
    content: Option<String>,
}

/// The IPC payload the canvas page posts after each render, listing any errors
/// captured during transpile/render (empty means it rendered cleanly).
#[derive(Deserialize)]
struct CanvasErrorReport {
    errors: Vec<String>,
}

impl SurfaceProvider for CanvasProvider {
    fn kind(&self) -> &'static str {
        "canvas"
    }

    fn open(
        &self,
        params: serde_json::Value,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<SurfaceId> {
        let params: CanvasParams = serde_json::from_value(params).unwrap_or_default();
        let title: SharedString = params.title.unwrap_or_else(|| "Canvas".to_string()).into();
        let path = PathBuf::from(
            params
                .path
                .ok_or_else(|| anyhow!("canvas open requires a file `path`"))?,
        );

        let project = workspace.project().clone();
        let view = cx.new(|cx| CanvasView::new(title, path, project, window, cx));
        workspace.active_pane().update(cx, |pane, cx| {
            pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
        });
        let id = register_canvas(&view, window.window_handle(), cx);
        Ok(SurfaceId(id))
    }

    fn update(&self, id: SurfaceId, params: serde_json::Value, cx: &mut App) -> Result<()> {
        let params: CanvasParams = serde_json::from_value(params).unwrap_or_default();
        let content = params
            .content
            .ok_or_else(|| anyhow!("canvas update requires `content`"))?;
        let view =
            canvas_by_id(id.0, cx).ok_or_else(|| anyhow!("no open canvas with id {}", id.0))?;
        let path = view.read(cx).path.clone();
        std::fs::write(&path, &content)
            .map_err(|err| anyhow!("failed to write {}: {err}", path.display()))?;
        view.update(cx, |view, cx| view.set_content(&content, cx));
        Ok(())
    }

    fn focus(
        &self,
        id: SurfaceId,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<bool> {
        let Some(view) = canvas_by_id(id.0, cx) else {
            return Ok(false);
        };
        let activated = workspace.activate_item(&view, true, true, window, cx);
        if activated && let Some(registry) = try_global_mut::<CanvasRegistry>(cx) {
            registry.last_focused = Some(id.0);
        }
        Ok(activated)
    }
}

/// The directory canvases live in: `~/.agents/canvases/` — outside any repo, a
/// sibling of `~/.agents/skills/`.
fn canvases_dir() -> PathBuf {
    paths::home_dir().join(".agents").join("canvases")
}

/// Ensures `~/.agents/canvases/` exists with the TS project scaffold
/// (`tsconfig.json` + `canvas.d.ts`) so the language server lints canvas files.
/// `canvas.d.ts` is rewritten each time to stay in sync with the SDK.
fn scaffold_canvas_project() {
    let dir = canvases_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::error!("canvas: failed to create {}: {err}", dir.display());
        return;
    }
    let tsconfig = dir.join("tsconfig.json");
    if !tsconfig.exists()
        && let Err(err) = std::fs::write(&tsconfig, TSCONFIG_JSON)
    {
        log::error!("canvas: failed to write tsconfig.json: {err}");
    }
    let dts = dir.join("canvas.d.ts");
    if let Err(err) = std::fs::write(&dts, CANVAS_DTS) {
        log::error!("canvas: failed to write canvas.d.ts: {err}");
    }
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in title.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "canvas".to_string()
    } else {
        slug
    }
}

/// Scaffolds the project (tsconfig + ambient types) and ensures
/// `~/.agents/canvases/<slug>.canvas.tsx` exists, writing `default_content` only
/// if the file is new (an existing canvas with the same title is left intact so
/// it can be reopened by identifier). Returns the file path.
fn prepare_canvas_file(title: &str, default_content: &str) -> Result<PathBuf> {
    scaffold_canvas_project();
    let path = canvases_dir().join(format!("{}.canvas.tsx", slugify(title)));
    if !path.exists() {
        std::fs::write(&path, default_content)
            .map_err(|err| anyhow!("failed to write {}: {err}", path.display()))?;
    }
    Ok(path)
}

/// A minimal starter canvas written when opening a title that doesn't exist yet.
fn starter_canvas(title: &str) -> String {
    let heading = title.replace(['{', '}'], "");
    format!(
        "function Canvas() {{\n  return (\n    <Page>\n      <h1>{heading}</h1>\n      <p>Edit this file to build the canvas.</p>\n    </Page>\n  );\n}}\n"
    )
}

/// Opens a canvas viewer for `path` in the active workspace (from an `&mut App`).
fn open_canvas_surface(title: &str, path: &Path, cx: &mut App) -> Result<SurfaceId> {
    let (workspace, window) =
        active_workspace(cx).ok_or_else(|| anyhow!("no active workspace to open a canvas in"))?;
    let params = serde_json::json!({ "title": title, "path": path.to_string_lossy() });
    let id = window.update(cx, |_root, window, cx| {
        workspace.update(cx, |workspace, cx| {
            surface::open_surface("canvas", params, workspace, window, cx)
        })?
    })??;
    Ok(id)
}

/// The id of a live (open) canvas showing `path`, if any.
fn live_canvas_id_for_path(path: &Path, cx: &App) -> Option<u64> {
    let registry = cx.try_global::<CanvasRegistry>()?;
    registry.instances.iter().find_map(|(id, weak)| {
        weak.upgrade()
            .filter(|view| view.read(cx).path == path)
            .map(|_| *id)
    })
}

/// Opens a canvas, or focuses the existing tab if one is already showing `path`
/// (so opening the same canvas twice doesn't create duplicate tabs).
fn open_or_focus_canvas(title: &str, path: &Path, cx: &mut App) -> Result<SurfaceId> {
    if let Some(id) = live_canvas_id_for_path(path, cx) {
        focus_canvas(id, cx)?;
        return Ok(SurfaceId(id));
    }
    open_canvas_surface(title, path, cx)
}

/// Registers `~/.agents/canvases/` as a *non-visible* worktree of the active
/// workspace's project, so the agent's `edit_file` and the language server
/// (linting) work on canvas files without the folder cluttering the project
/// panel. Awaits readiness so the agent can edit immediately after.
async fn ensure_canvas_worktree(cx: &mut gpui::AsyncApp) -> Result<()> {
    let dir = canvases_dir();
    let Some((workspace, _window)) = cx.update(|cx| active_workspace(cx)) else {
        anyhow::bail!("no active workspace");
    };
    let task = workspace.update(cx, |workspace, cx| {
        workspace.project().update(cx, |project, cx| {
            project.find_or_create_worktree(&dir, false, cx)
        })
    })?;
    task.await?;
    Ok(())
}

/// Brings the canvas with `id` to the foreground (activates its tab) in the
/// active workspace.
fn focus_canvas(id: u64, cx: &mut App) -> Result<()> {
    let view = canvas_by_id(id, cx).ok_or_else(|| anyhow!("no open canvas with id {id}"))?;
    let (workspace, window) =
        active_workspace(cx).ok_or_else(|| anyhow!("no active workspace to focus a canvas in"))?;
    let activated = window.update(cx, |_root, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.activate_item(&view, true, true, window, cx)
        })
    })??;
    anyhow::ensure!(activated, "canvas {id} is not in the active workspace");
    if let Some(registry) = try_global_mut::<CanvasRegistry>(cx) {
        registry.last_focused = Some(id);
    }
    Ok(())
}

/// Cycles to the next open canvas (by id, wrapping) and activates it. Driven by
/// the `canvas: focus next canvas` action so canvases can be navigated without
/// relying on the tab bar.
fn focus_next_canvas(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let canvases = live_canvases(cx);
    if canvases.is_empty() {
        return;
    }
    let last_focused = cx
        .try_global::<CanvasRegistry>()
        .and_then(|registry| registry.last_focused);
    let next_id = match last_focused {
        Some(last) => canvases
            .iter()
            .map(|(id, _)| *id)
            .find(|id| *id > last)
            .or_else(|| canvases.first().map(|(id, _)| *id)),
        None => canvases.first().map(|(id, _)| *id),
    };
    let Some(next_id) = next_id else {
        return;
    };
    let Some(view) = canvas_by_id(next_id, cx) else {
        return;
    };
    workspace.activate_item(&view, true, true, window, cx);
    if cx.has_global::<CanvasRegistry>() {
        cx.global_mut::<CanvasRegistry>().last_focused = Some(next_id);
    }
}

fn try_global_mut<G: Global>(cx: &mut App) -> Option<&mut G> {
    cx.has_global::<G>().then(|| cx.global_mut::<G>())
}

// --- Agent tools -------------------------------------------------------------

/// Opens a canvas: a panel that renders a React component to present
/// information to the user (reports, dashboards, summaries, charts).
///
/// Canvases are files. This tool opens the canvas for `title` (creating a
/// starter file if one doesn't exist yet), or focuses its tab if it's already
/// open, and returns its file path. You then author it by editing that
/// `.canvas.tsx` file directly with your normal file tools (`edit_file`) — it
/// hot-reloads on save. Re-opening the same title reopens the existing file.
/// Lint your edits with the `diagnostics` tool, and call `canvas_errors` to
/// confirm it renders.
///
/// The file must define exactly one top-level `function Canvas() { ... }`
/// returning the UI. IMPORTANT RULES for its contents:
/// - Do NOT use `import` or `export`, and do NOT write TypeScript type
///   annotations. React and the component library are provided as globals and
///   only JSX is transpiled.
///
/// Components available as globals (no import needed):
/// - `<Page>…</Page>`: root wrapper with readable typography (Tailwind `prose`).
///   Put prose elements (h1/h2/p/ul/ol/blockquote/code/strong/em) directly inside
///   and they are styled for reading automatically.
/// - `<Card title="…">…</Card>`
/// - `<Stat label="…" value="…" delta="…" tone="up|down" />`
/// - `<Grid cols={n}>…</Grid>`, `<Stack gap={n}>…</Stack>`, `<Row gap={n}>…</Row>`
/// - `<Table headers={[…]} rows={[[…], …]} align={["left"|"right", …]} />`
/// - `<BarChart data={[{ label, value }, …]} />`
/// - `<Button variant="primary" onClick={fn}>…</Button>`, `<Badge tone="success|danger">…</Badge>`
/// - `useHostTheme()` -> `{ kind: "light" | "dark", name: string | null, colors: Record<string, string> }`
///
/// You may also use Tailwind utility classes via `className="…"` for layout/color.
/// Keep it clean and readable: neutral grays, a single accent color, no gradients
/// or drop shadows.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CanvasOpenToolInput {
    /// Short title shown on the canvas tab. Also identifies the canvas: opening
    /// the same title reopens the same file.
    title: String,
}

struct CanvasOpenTool {
    /// The session that owns this tool instance, recorded so the created canvas
    /// can be associated with (and reopened from) the conversation.
    session_id: acp::SessionId,
}

impl AgentTool for CanvasOpenTool {
    type Input = CanvasOpenToolInput;
    type Output = String;

    const NAME: &'static str = "canvas_open";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Open canvas: {}", input.title).into(),
            Err(_) => "Open canvas".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let session_id = self.session_id.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|err| err.to_string())?;
            // Create a starter file only if this canvas doesn't exist yet; an
            // existing canvas with the same title is reopened as-is.
            let path = prepare_canvas_file(&input.title, &starter_canvas(&input.title))
                .map_err(|err| err.to_string())?;
            // Make the canvases dir a (non-visible) worktree so `edit_file` and the
            // language server work on the file before the agent edits it.
            ensure_canvas_worktree(cx).await.map_err(|err| err.to_string())?;
            let path_string = path.to_string_lossy().to_string();
            let id = cx
                .update(|cx| open_or_focus_canvas(&input.title, &path, cx))
                .map_err(|err| err.to_string())?;
            // Associate the canvas with this conversation so the thread view can
            // offer to (re)open it. `params` mirror what `CanvasProvider::open`
            // expects, so a closed canvas can be reopened from disk.
            let title = input.title.clone();
            cx.update(|cx| {
                let params = serde_json::json!({ "title": &title, "path": &path_string });
                surface::record_session_surface(
                    surface::SessionKey::new(session_id.0.to_string()),
                    "canvas",
                    title,
                    params,
                    id,
                    cx,
                );
            });
            Ok(format!(
                "Opened canvas \"{}\" (id {}). It is the file `{}` — author it by editing that file directly with `edit_file` (it hot-reloads on save). Lint your edits with `diagnostics`, then call `canvas_errors` with id {} to confirm it renders without errors.",
                input.title,
                id.0,
                path.display(),
                id.0
            ))
        })
    }
}

/// Lists the open canvases as JSON `[{ "id", "title" }]` so the agent can decide
/// which one to focus.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CanvasListToolInput {}

struct CanvasListTool;

impl AgentTool for CanvasListTool {
    type Input = CanvasListToolInput;
    type Output = String;

    const NAME: &'static str = "canvas_list";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List canvases".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let _input = input.recv().await.map_err(|err| err.to_string())?;
            let canvases = cx.update(|cx| live_canvases(cx));
            let json = canvases
                .into_iter()
                .map(|(id, title)| serde_json::json!({ "id": id, "title": title.to_string() }))
                .collect::<Vec<_>>();
            serde_json::to_string(&json).map_err(|err| err.to_string())
        })
    }
}

/// Reports JavaScript/render errors for a canvas, so you can verify a canvas you
/// created or edited actually renders. Call this after `canvas_open` or after
/// editing a `.canvas.tsx` file to check for problems and fix them.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CanvasErrorsToolInput {
    /// The id of the canvas to check (from `canvas_open` or `canvas_list`).
    id: u64,
}

struct CanvasErrorsTool;

impl AgentTool for CanvasErrorsTool {
    type Input = CanvasErrorsToolInput;
    type Output = String;

    const NAME: &'static str = "canvas_errors";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Check canvas {} for errors", input.id).into(),
            Err(_) => "Check canvas for errors".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|err| err.to_string())?;
            // Give a pending hot-reload (the file watcher polls ~600ms) time to
            // re-render and report its error state before we read it.
            cx.background_executor()
                .timer(Duration::from_millis(900))
                .await;
            let errors = cx
                .update(|cx| {
                    canvas_by_id(input.id, cx).map(|view| view.read(cx).render_errors())
                })
                .ok_or_else(|| format!("no open canvas with id {}", input.id))?;
            if errors.is_empty() {
                Ok(format!("Canvas {} rendered with no errors.", input.id))
            } else {
                Ok(format!(
                    "Canvas {} reported {} error(s):\n{}",
                    input.id,
                    errors.len(),
                    errors.join("\n")
                ))
            }
        })
    }
}

// --- The canvas view (a workspace Item) --------------------------------------

/// What the canvas tab is currently showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CanvasMode {
    /// The rendered WebView output.
    Rendered,
    /// The `.canvas.tsx` source that produced it, for inspection.
    Code,
}

struct CanvasView {
    title: SharedString,
    path: PathBuf,
    focus_handle: FocusHandle,
    // The window this canvas lives in, used to scope transparency to windows
    // that actually host a canvas.
    window_handle: AnyWindowHandle,
    // Used to open the backing file as a real, syntax-highlighted buffer for the
    // code view.
    project: Entity<Project>,
    // Whether the tab shows the rendered canvas or its source.
    mode: CanvasMode,
    // A read-only editor over the backing file, lazily created the first time
    // the code view is shown.
    code_editor: Option<Entity<Editor>>,
    // The current full HTML document, shared with the WebView's custom-protocol
    // handler so updates just mutate this and reload.
    #[cfg(target_os = "macos")]
    document: Rc<RefCell<String>>,
    // Kept alive for the life of the tab; dropping it removes the native view.
    // An `Rc` so the per-frame `canvas()` paint closure can hold a clone.
    #[cfg(target_os = "macos")]
    webview: Option<Rc<wry::WebView>>,
    // The host theme payload (`window.__zedTheme`) the document was last built
    // with, kept to detect actual theme changes among unrelated settings churn.
    #[cfg(target_os = "macos")]
    theme_json: String,
    // Rebuilds the document when the host theme changes; dropped with the view.
    #[cfg(target_os = "macos")]
    _theme_subscription: Subscription,
    // The latest render error list reported by the page over IPC (empty means it
    // rendered cleanly). Read by the `canvas_errors` tool.
    #[cfg(target_os = "macos")]
    errors: Rc<RefCell<Vec<String>>>,
    // Polls the backing file and hot-reloads on change; cancelled when dropped.
    _watch_task: Task<()>,
}

impl CanvasView {
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn new(
        title: SharedString,
        path: PathBuf,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let window_handle = window.window_handle();
        #[cfg(target_os = "macos")]
        let theme_json = canvas_theme_json(cx);
        #[cfg(target_os = "macos")]
        let document = Rc::new(RefCell::new(canvas_document(
            title.as_ref(),
            &content,
            &theme_json,
        )));
        #[cfg(target_os = "macos")]
        let errors = Rc::new(RefCell::new(Vec::new()));
        #[cfg(target_os = "macos")]
        let webview = attach_webview(window, document.clone(), errors.clone()).map(Rc::new);
        #[cfg(target_os = "macos")]
        let theme_subscription =
            cx.observe_global::<settings::SettingsStore>(|this, cx| this.refresh_theme(cx));
        let watch_task = Self::spawn_watch(path.clone(), cx);

        Self {
            title,
            path,
            focus_handle: cx.focus_handle(),
            window_handle,
            project,
            mode: CanvasMode::Rendered,
            code_editor: None,
            #[cfg(target_os = "macos")]
            document,
            #[cfg(target_os = "macos")]
            webview,
            #[cfg(target_os = "macos")]
            theme_json,
            #[cfg(target_os = "macos")]
            _theme_subscription: theme_subscription,
            #[cfg(target_os = "macos")]
            errors,
            _watch_task: watch_task,
        }
    }

    /// Polls the backing file's mtime and hot-reloads the canvas when it changes
    /// on disk (e.g. after the agent edits it with `edit_file`).
    fn spawn_watch(path: PathBuf, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            fn mtime(path: &Path) -> Option<std::time::SystemTime> {
                std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
            }
            let mut last = mtime(&path);
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(600))
                    .await;
                let current = mtime(&path);
                if current == last {
                    continue;
                }
                last = current;
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if this
                    .update(cx, |this, cx| this.set_content(&content, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn set_content(&mut self, content: &str, _cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            *self.document.borrow_mut() =
                canvas_document(self.title.as_ref(), content, &self.theme_json);
            if let Some(webview) = self.webview.as_ref()
                && let Err(err) = webview.load_url("zedcanvas://localhost/")
            {
                log::error!("canvas: load_url failed: {err}");
            }
        }
    }

    /// The latest render errors reported by the page (empty means it rendered
    /// cleanly). Always empty off macOS, where there is no WebView.
    fn render_errors(&self) -> Vec<String> {
        #[cfg(target_os = "macos")]
        {
            self.errors.borrow().clone()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }

    /// Switches between the rendered canvas and its source. Hides the WebView in
    /// code mode so the (opaque) code view isn't drawn over a live WebView; the
    /// rendered mode's `canvas()` positioner shows it again on switch back.
    fn set_mode(&mut self, mode: CanvasMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        #[cfg(target_os = "macos")]
        if mode == CanvasMode::Code {
            if let Some(webview) = self.webview.as_ref()
                && let Err(err) = webview.set_visible(false)
            {
                log::error!("canvas: set_visible(false) failed: {err}");
            }
            // The code editor is opaque GPUI; stop passing mouse events through.
            window.set_mouse_passthrough_rects(Vec::new());
        }
        if mode == CanvasMode::Code && self.code_editor.is_none() {
            self.load_code_editor(window, cx);
        }
        cx.notify();
    }

    /// Opens the backing file as a project buffer and builds a read-only editor
    /// for it (syntax highlighting, scrolling, selection). Runs once, lazily.
    fn load_code_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let project = self.project.clone();
        let buffer_task =
            project.update(cx, |project, cx| project.open_local_buffer(&path, cx));
        cx.spawn_in(window, async move |this, cx| {
            let buffer = match buffer_task.await {
                Ok(buffer) => buffer,
                Err(err) => {
                    log::error!("canvas: failed to open source buffer: {err}");
                    return;
                }
            };
            let result = this.update_in(cx, |this, window, cx| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                this.code_editor = Some(editor);
                cx.notify();
            });
            if let Err(err) = result {
                log::error!("canvas: failed to install code editor: {err}");
            }
        })
        .detach();
    }

    /// Rebuilds the document with the latest host theme and reloads the WebView.
    /// No-op when the serialized theme is unchanged, since the settings observer
    /// fires on every settings change, not just theme changes.
    #[cfg(target_os = "macos")]
    fn refresh_theme(&mut self, cx: &mut Context<Self>) {
        let theme_json = canvas_theme_json(cx);
        if theme_json == self.theme_json {
            return;
        }
        self.theme_json = theme_json;
        let content = std::fs::read_to_string(&self.path).unwrap_or_default();
        self.set_content(&content, cx);
    }
}

impl Render for CanvasView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.mode {
            CanvasMode::Rendered => self.render_canvas_content(cx),
            CanvasMode::Code => self.render_code_view(cx).into_any_element(),
        };

        // A top toolbar strip holds the Canvas/Code toggle. It is opaque and
        // sits outside the transparency hole, so its clicks reach GPUI (the hole
        // region passes mouse events through to the WebView; see
        // `render_canvas_content`).
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().relative().child(content))
    }
}

impl CanvasView {
    /// The rendered-canvas content: a transparency hole that the native WebView
    /// shows through. The `canvas()` painter keeps the WebView aligned with the
    /// hole and registers the hole as a native mouse-passthrough region so the
    /// WebView receives scroll/selection/click events.
    fn render_canvas_content(&self, _cx: &mut Context<Self>) -> gpui::AnyElement {
        #[cfg(target_os = "macos")]
        {
            let webview = self.webview.clone();
            div()
                .size_full()
                .bg(gpui::transparency_hole())
                .child(
                    canvas(
                        |_bounds, _window, _cx| {},
                        move |bounds: Bounds<Pixels>, _, window, _cx| {
                            position_webview(webview.as_ref(), bounds);
                            window.set_mouse_passthrough_rects(vec![bounds]);
                        },
                    )
                    .size_full(),
                )
                .into_any_element()
        }
        #[cfg(not(target_os = "macos"))]
        {
            div().size_full().into_any_element()
        }
    }

    /// The opaque top strip containing the Canvas/Code toggle.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use theme::ActiveTheme as _;

        let view = cx.entity();
        let selected_index = match self.mode {
            CanvasMode::Rendered => 0,
            CanvasMode::Code => 1,
        };
        let toggle = ToggleButtonGroup::single_row(
            "canvas-mode",
            [
                ToggleButtonSimple::new("Canvas", {
                    let view = view.clone();
                    move |_, window, cx| {
                        view.update(cx, |this, cx| this.set_mode(CanvasMode::Rendered, window, cx));
                    }
                }),
                ToggleButtonSimple::new("Code", move |_, window, cx| {
                    view.update(cx, |this, cx| this.set_mode(CanvasMode::Code, window, cx));
                }),
            ],
        )
        .style(ToggleButtonGroupStyle::Filled)
        .label_size(LabelSize::Small)
        .selected_index(selected_index)
        .auto_width();

        div()
            .flex()
            .flex_row()
            .w_full()
            .items_center()
            .justify_end()
            .px_2()
            .py_1()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(toggle)
    }

    /// A read-only editor over the `.canvas.tsx` source, or a placeholder while
    /// the buffer is still opening.
    fn render_code_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use theme::ActiveTheme as _;

        let colors = cx.theme().colors();
        let container = div().size_full().bg(colors.editor_background);
        match &self.code_editor {
            Some(editor) => container.child(editor.clone()),
            None => container
                .text_color(colors.text_muted)
                .px_4()
                .py_3()
                .child("Loading source…"),
        }
    }
}

impl Focusable for CanvasView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for CanvasView {}

impl Item for CanvasView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn deactivated(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        // The WebView is a window-level sibling view, so it does not disappear on
        // its own when another tab becomes active; hide it explicitly. It is shown
        // again by `position_webview` the next time this tab paints.
        #[cfg(target_os = "macos")]
        if let Some(webview) = self.webview.as_ref()
            && let Err(err) = webview.set_visible(false)
        {
            log::error!("canvas: set_visible(false) failed: {err}");
        }
        // Stop passing mouse events through to the (now hidden) WebView region,
        // so the tab that replaces this one receives events normally.
        window.set_mouse_passthrough_rects(Vec::new());
    }
}

/// Positions the WebView over `bounds` (GPUI window coordinates, top-left origin)
/// and makes it visible. `wry` performs the AppKit Y-flip internally, and GPUI's
/// window origin matches the `contentView` origin, so the bounds map directly.
#[cfg(target_os = "macos")]
fn position_webview(webview: Option<&Rc<wry::WebView>>, bounds: Bounds<Pixels>) {
    let Some(webview) = webview else {
        return;
    };
    // One-shot diagnostic: is the canvas laid out below the pane's tab bar, or
    // flush at the pane top (i.e. is the WebView covering the tab strip)?
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log::info!(
                "canvas: first webview bounds origin=({}, {}) size=({} x {})",
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            );
        }
    }
    let rect = wry::Rect {
        position: wry::dpi::LogicalPosition::new(
            f32::from(bounds.origin.x) as f64,
            f32::from(bounds.origin.y) as f64,
        )
        .into(),
        size: wry::dpi::LogicalSize::new(
            f32::from(bounds.size.width) as f64,
            f32::from(bounds.size.height) as f64,
        )
        .into(),
    };
    if let Err(err) = webview.set_bounds(rect) {
        log::error!("canvas: set_bounds failed: {err}");
    }
    if let Err(err) = webview.set_visible(true) {
        log::error!("canvas: set_visible(true) failed: {err}");
    }
}

#[cfg(target_os = "macos")]
fn attach_webview(
    window: &Window,
    document: Rc<RefCell<String>>,
    errors: Rc<RefCell<Vec<String>>>,
) -> Option<wry::WebView> {
    use std::borrow::Cow;
    use wry::WebViewBuilder;
    use wry::http::{Response, header::CONTENT_TYPE};

    let Some(content_view) = window_content_view(window) else {
        log::error!("canvas: could not resolve contentView; not attaching");
        return None;
    };
    let parent = ContentViewHandle { view: content_view };

    // `new_as_child` adds the WebView as a subview of `contentView` (sibling of
    // GPUI's Metal view), which is the stable attachment point. Initial bounds are
    // a placeholder; the first `canvas()` paint repositions it.
    let initial = wry::Rect {
        position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
        size: wry::dpi::LogicalSize::new(640.0, 480.0).into(),
    };

    // Serve the document from a custom scheme so the page has a real origin. A
    // `load_html` page is opaque-origin, which scrubs script errors to bare
    // "Script error." and breaks the React/Babel runtime.
    let protocol_document = document;
    let protocol = move |_request: wry::http::Request<Vec<u8>>| {
        let html = protocol_document.borrow().clone().into_bytes();
        Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .body(Cow::Owned(html))
            .unwrap_or_else(|_| Response::new(Cow::Borrowed(&b""[..])))
    };

    // The page reports its render error state over IPC after each (re)render; we
    // store it so the `canvas_errors` tool can report it back to the agent.
    let ipc_errors = errors;
    let ipc_handler = move |request: wry::http::Request<String>| {
        let body = request.body();
        match serde_json::from_str::<CanvasErrorReport>(body) {
            Ok(report) => *ipc_errors.borrow_mut() = report.errors,
            Err(err) => log::error!("canvas: bad IPC error report: {err}"),
        }
    };

    match WebViewBuilder::new_as_child(&parent)
        .with_bounds(initial)
        .with_devtools(false)
        .with_initialization_script(
            "window.addEventListener('contextmenu', function (e) { e.preventDefault(); }, true);",
        )
        .with_custom_protocol("zedcanvas".into(), protocol)
        .with_ipc_handler(ipc_handler)
        .with_url("zedcanvas://localhost/")
        .build()
    {
        Ok(webview) => {
            log::info!("canvas: WebView attached (custom protocol)");
            // The WebView is a sibling of GPUI's Metal view under `contentView`.
            // `new_as_child` stacks it *above* the Metal view, which would cover
            // GPUI's menus and overlays. Instead, layer it *below* the Metal
            // view so the WebView only shows through where the canvas tab
            // punches a transparency hole (see `CanvasView::render`); everything
            // GPUI draws then composites on top. The window's surface is made
            // non-opaque separately by `refresh_window_transparency`.
            order_webview_below_native_view(window);
            Some(webview)
        }
        Err(err) => {
            log::error!("canvas: failed to build WebView: {err:#}");
            None
        }
    }
}

/// Raises GPUI's Metal `native_view` above its siblings under `contentView`,
/// which leaves the just-attached WebView ordered below it. Re-adding an
/// existing subview with `addSubview:positioned:relativeTo:` reorders it in
/// place rather than duplicating it.
#[cfg(target_os = "macos")]
fn order_webview_below_native_view(window: &Window) {
    use objc::{msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `NSWindowOrderingMode::NSWindowAbove`.
    const NS_WINDOW_ABOVE: isize = 1;

    let Ok(window_handle) = HasWindowHandle::window_handle(window) else {
        log::error!("canvas: could not resolve window handle for reordering");
        return;
    };
    let RawWindowHandle::AppKit(handle) = window_handle.as_raw() else {
        return;
    };
    let native_view = handle.ns_view.as_ptr() as *mut Object;

    // SAFETY: `native_view` is a live NSView owned by the GPUI window; `window`
    // and `contentView` are standard AppKit accessors returning borrowed
    // objects. `addSubview:positioned:relativeTo:` reorders the existing
    // subview without changing ownership.
    unsafe {
        let ns_window: *mut Object = msg_send![native_view, window];
        if ns_window.is_null() {
            log::error!("canvas: native view has no window; cannot reorder");
            return;
        }
        let content_view: *mut Object = msg_send![ns_window, contentView];
        if content_view.is_null() {
            return;
        }
        let _: () = msg_send![
            content_view,
            addSubview: native_view
            positioned: NS_WINDOW_ABOVE
            relativeTo: std::ptr::null_mut::<Object>()
        ];
    }
}

/// Walks from GPUI's `native_view` (the NSView its window handle points at) up to
/// the enclosing window's `contentView`.
#[cfg(target_os = "macos")]
fn window_content_view(window: &Window) -> Option<std::ptr::NonNull<std::ffi::c_void>> {
    use objc::{msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `window.window_handle()` would resolve to GPUI's inherent method (returning
    // an `AnyWindowHandle`); call the raw-window-handle trait method explicitly.
    let window_handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = window_handle.as_raw() else {
        return None;
    };
    let native_view = handle.ns_view.as_ptr() as *mut Object;

    // SAFETY: `native_view` is a live NSView owned by the GPUI window; `window` and
    // `contentView` are standard AppKit accessors returning borrowed objects.
    unsafe {
        let ns_window: *mut Object = msg_send![native_view, window];
        if ns_window.is_null() {
            return None;
        }
        let content_view: *mut Object = msg_send![ns_window, contentView];
        std::ptr::NonNull::new(content_view as *mut std::ffi::c_void)
    }
}

/// Minimal `HasWindowHandle` wrapper so we can point `wry` at an arbitrary NSView
/// (the window's `contentView`) rather than GPUI's Metal view.
#[cfg(target_os = "macos")]
struct ContentViewHandle {
    view: std::ptr::NonNull<std::ffi::c_void>,
}

#[cfg(target_os = "macos")]
impl raw_window_handle::HasWindowHandle for ContentViewHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::AppKitWindowHandle::new(self.view);
        // SAFETY: `view` points at the window's `contentView`, which outlives this
        // handle (it is owned by the window we just attached to).
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::AppKit(
                handle,
            ))
        })
    }
}

/// Wraps a canvas component (JSX defining `function Canvas() { ... }`) in the
/// React + Tailwind + `prose` runtime shell, ready to hand to the WebView.
/// `theme_json` is the host theme payload injected as `window.__zedTheme`.
fn canvas_document(title: &str, body_tsx: &str, theme_json: &str) -> String {
    let safe_title = title.replace('<', "&lt;").replace('>', "&gt;");
    // The body is embedded inside a <script>; neutralize any literal </script>.
    let safe_body = body_tsx.replace("</script>", "<\\/script>");
    CANVAS_SHELL
        .replace("CANVAS_TITLE_PLACEHOLDER", &safe_title)
        .replace("\"CANVAS_THEME_PLACEHOLDER\"", theme_json)
        .replace("// CANVAS_BODY_PLACEHOLDER", &safe_body)
}

/// Serializes the active Zed theme into the JSON payload exposed to canvases as
/// `window.__zedTheme` (and read by `useHostTheme()`). Colors are CSS
/// `rgba(...)` strings so they can be dropped straight into styles.
fn canvas_theme_json(cx: &App) -> String {
    use theme::ActiveTheme as _;

    let theme = cx.theme();
    let colors = theme.colors();
    let status = theme.status();
    let kind = if theme.appearance().is_light() {
        "light"
    } else {
        "dark"
    };

    serde_json::json!({
        "kind": kind,
        "name": theme.name.to_string(),
        "colors": {
            "background": css_color(colors.background),
            "surface": css_color(colors.elevated_surface_background),
            "panel": css_color(colors.panel_background),
            "element": css_color(colors.element_background),
            "border": css_color(colors.border),
            "text": css_color(colors.text),
            "textMuted": css_color(colors.text_muted),
            "accent": css_color(colors.text_accent),
            "error": css_color(status.error),
            "warning": css_color(status.warning),
            "success": css_color(status.success),
            "info": css_color(status.info),
        }
    })
    .to_string()
    // Injected inside a <script>; escape `<` so a theme name can't break out.
    .replace('<', "\\u003c")
}

/// Formats a GPUI color as a CSS `rgba(...)` string.
fn css_color(color: gpui::Hsla) -> String {
    let rgba = gpui::Rgba::from(color);
    let to_u8 = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({}, {}, {}, {:.3})",
        to_u8(rgba.r),
        to_u8(rgba.g),
        to_u8(rgba.b),
        rgba.a.clamp(0.0, 1.0)
    )
}

/// Default canvas shown by the `canvas: open canvas spike` action; also a
/// reference for the component API.
const DEFAULT_CANVAS: &str = r##"
function Canvas() {
  const { kind, name, colors } = useHostTheme();
  return (
    <Page>
      <h1>Canvas runtime is live</h1>
      <p>
        This canvas is a React component rendered with Tailwind and the{" "}
        <code>prose</code> typography plugin. Host theme:{" "}
        <strong>{name || kind}</strong> ({kind}).
      </p>
      <Grid cols={3}>
        <Card title="Revenue"><Stat label="This month" value="$48.2k" delta="+12%" tone="up" /></Card>
        <Card title="Active users"><Stat label="Today" value="1,284" delta="-3%" tone="down" /></Card>
        <Card title="NPS"><Stat label="Score" value="62" /></Card>
      </Grid>
      <h2>Sample bar chart</h2>
      <BarChart data={[{label:"A",value:8},{label:"B",value:14},{label:"C",value:5},{label:"D",value:11}]} accent={colors.accent} />
      <h2>Table</h2>
      <Table headers={["Item","Count"]} rows={[["Alpha","12"],["Beta","34"],["Gamma","7"]]} align={["left","right"]} />
      <Row gap={3}>
        <Button variant="primary">Primary</Button>
        <Button>Secondary</Button>
        <Badge tone="success">ok</Badge>
      </Row>
    </Page>
  );
}
"##;

/// React + Tailwind (+ typography) + Babel runtime shell. `CANVAS_TITLE_PLACEHOLDER`
/// and `// CANVAS_BODY_PLACEHOLDER` are substituted by [`canvas_document`]. The
/// SDK components are defined here and exposed as globals; the user's canvas is
/// transpiled in-browser by Babel and mounted as `<Canvas/>`.
const CANVAS_SHELL: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>CANVAS_TITLE_PLACEHOLDER</title>
<script>
  // Capture ANY uncaught error (including failures inside Babel's transform or
  // the compiled scripts, which are outside our try/catch) for the diagnostic,
  // and report the current error list to the host (Zed) over IPC so the agent
  // can see when a canvas it wrote is broken.
  window.__canvasErrors = [];
  window.__reportCanvasErrors = function () {
    try {
      if (window.ipc && window.ipc.postMessage) {
        window.ipc.postMessage(JSON.stringify({ errors: window.__canvasErrors || [] }));
      }
    } catch (e) {}
  };
  window.addEventListener('error', function (e) {
    window.__canvasErrors.push(
      String((e && e.error && e.error.stack) || (e && e.message) || e)
      + (e && e.filename ? (' @ ' + e.filename + ':' + e.lineno) : '')
    );
    window.__reportCanvasErrors();
  });
  window.addEventListener('unhandledrejection', function (e) {
    window.__canvasErrors.push('unhandledrejection: ' + String((e && e.reason && e.reason.stack) || (e && e.reason) || e));
    window.__reportCanvasErrors();
  });
</script>
<script>
  // Host theme injected by Zed. `useHostTheme()` reads this object; the bootstrap
  // applies it as a `dark` class (so Tailwind `dark:` variants follow Zed's
  // active theme rather than the OS setting) and as `--zed-*` CSS variables.
  window.__zedTheme = "CANVAS_THEME_PLACEHOLDER";
  (function () {
    var t = window.__zedTheme;
    if (!t) return;
    document.documentElement.classList.toggle('dark', t.kind === 'dark');
    var colors = t.colors || {};
    var root = document.documentElement.style;
    for (var key in colors) {
      if (Object.prototype.hasOwnProperty.call(colors, key)) {
        root.setProperty('--zed-' + key, colors[key]);
      }
    }
  })();
</script>
<script src="https://unpkg.com/react@18/umd/react.production.min.js" crossorigin></script>
<script src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js" crossorigin></script>
<script src="https://cdn.tailwindcss.com?plugins=typography"></script>
<script src="https://unpkg.com/@babel/standalone@7/babel.min.js"></script>
<script>
  tailwind.config = {
    darkMode: 'class',
    theme: { extend: { fontFamily: { serif: ['ETBembo', '"Palatino Linotype"', 'Palatino', 'Georgia', 'serif'] } } }
  };
</script>
<style>
  html, body { margin: 0; height: 100%; }
  /* Driven by the host theme via `--zed-*` variables (see the theme bootstrap),
     with light defaults as a fallback when no host theme is present. */
  body { background: var(--zed-background, #fffff8); color: var(--zed-text, #111111); }
  /* Make code / equation blocks theme-aware even outside `prose` (e.g. inside a
     Card), so agent-authored blocks follow the host theme automatically. */
  pre, code, kbd, samp { background: var(--zed-element, rgba(0, 0, 0, 0.06)); border-radius: 4px; }
  #root { min-height: 100%; }
</style>
</head>
<body class="font-serif">
<div id="root" class="px-8 py-6"></div>

<script type="text/plain" id="canvas-sdk">
function cx() { return Array.prototype.slice.call(arguments).filter(Boolean).join(' '); }
function useHostTheme() {
  var t = (typeof window !== 'undefined' && window.__zedTheme) || null;
  var prefersDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
  return {
    kind: t ? t.kind : (prefersDark ? 'dark' : 'light'),
    name: t ? t.name : null,
    colors: (t && t.colors) || {},
  };
}
function Page({ children, prose = true, className }) {
  return <div className={cx('mx-auto max-w-3xl', prose && 'prose dark:prose-invert prose-headings:font-serif', className)}>{children}</div>;
}
function Stack({ children, gap = 4, className }) { return <div className={cx('flex flex-col', 'gap-' + gap, className)}>{children}</div>; }
function Row({ children, gap = 4, className }) { return <div className={cx('flex flex-row items-center flex-wrap', 'gap-' + gap, className)}>{children}</div>; }
function Grid({ children, cols = 2, gap = 4, className }) { return <div className={cx('grid not-prose', 'grid-cols-' + cols, 'gap-' + gap, className)}>{children}</div>; }
function Card({ title, children, className }) {
  return <div className={cx('not-prose rounded-lg border border-black/10 dark:border-white/15 p-4', className)}>
    {title ? <div className="text-sm font-medium text-black/60 dark:text-white/60 mb-2">{title}</div> : null}
    {children}
  </div>;
}
function Stat({ label, value, delta, tone }) {
  const t = tone === 'up' ? 'text-emerald-600 dark:text-emerald-400' : tone === 'down' ? 'text-rose-600 dark:text-rose-400' : 'text-black/50 dark:text-white/50';
  return <div className="not-prose">
    <div className="text-sm text-black/60 dark:text-white/60">{label}</div>
    <div className="text-3xl font-semibold tabular-nums">{value}</div>
    {delta != null ? <div className={cx('text-sm', t)}>{delta}</div> : null}
  </div>;
}
function Button({ children, onClick, variant }) {
  const base = 'not-prose inline-flex items-center rounded-md border px-3 py-1.5 text-sm font-medium cursor-pointer';
  const v = variant === 'primary' ? 'bg-black text-white border-black dark:bg-white dark:text-black' : 'border-black/15 dark:border-white/20 hover:bg-black/5 dark:hover:bg-white/10';
  return <button className={cx(base, v)} onClick={onClick}>{children}</button>;
}
function Badge({ children, tone }) {
  const t = tone === 'danger' ? 'bg-rose-500/15 text-rose-700 dark:text-rose-300' : tone === 'success' ? 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-300' : 'bg-black/10 dark:bg-white/15';
  return <span className={cx('not-prose inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium', t)}>{children}</span>;
}
function Table({ headers = [], rows = [], align = [] }) {
  const a = (i) => align[i] === 'right' ? 'text-right tabular-nums' : 'text-left';
  return <table className="not-prose w-full text-sm border-collapse">
    <thead><tr className="border-b border-black/15 dark:border-white/20">
      {headers.map((h, i) => <th key={i} className={cx('py-1 pr-4 font-medium text-black/60 dark:text-white/60', a(i))}>{h}</th>)}
    </tr></thead>
    <tbody>{rows.map((r, ri) => <tr key={ri} className="border-b border-black/5 dark:border-white/10">
      {r.map((c, ci) => <td key={ci} className={cx('py-1 pr-4 align-top', a(ci))}>{c}</td>)}
    </tr>)}</tbody>
  </table>;
}
function BarChart({ data = [], height = 220, accent = '#e41a1c' }) {
  const max = Math.max(1, ...data.map((d) => d.value));
  const n = data.length || 1;
  const bw = 100 / n;
  return <svg className="not-prose w-full" viewBox="0 0 100 60" preserveAspectRatio="none" role="img" aria-label="bar chart" style={{ height: height }}>
    {data.map((d, i) => {
      const h = (d.value / max) * 50;
      return <rect key={i} x={i * bw + bw * 0.15} y={54 - h} width={bw * 0.7} height={h} fill={i === 0 ? accent : '#9ca3af'} />;
    })}
  </svg>;
}
Object.assign(window, { cx, useHostTheme, Page, Stack, Row, Grid, Card, Stat, Button, Badge, Table, BarChart });
</script>

<script type="text/plain" id="canvas-body">
// CANVAS_BODY_PLACEHOLDER
</script>

<script>
  // Transpile the SDK + canvas with Babel's stable transform API and run them via
  // indirect eval (global scope). We avoid Babel's transformScriptTags / dynamic
  // <script> injection, which throws (appendChild) inside WKWebView.
  (function () {
    function fail(msg) {
      window.__canvasErrors.push(msg);
      window.__reportCanvasErrors();
      var root = document.getElementById('root');
      if (root) {
        root.innerHTML = '<pre style="white-space:pre-wrap;color:#c0392b;font:13px ui-monospace,monospace;padding:1rem">' + msg + '</pre>';
      }
    }
    try {
      if (!window.Babel || !Babel.transform) { return fail('Babel.transform unavailable'); }
      var sdk = document.getElementById('canvas-sdk').textContent;
      var body = document.getElementById('canvas-body').textContent;
      (0, eval)(Babel.transform(sdk, { presets: ['react', 'typescript'], filename: 'sdk.tsx' }).code);
      (0, eval)(Babel.transform(body, { presets: ['react', 'typescript'], filename: 'canvas.tsx' }).code);
      var element = (typeof Canvas !== 'undefined')
        ? React.createElement(Canvas)
        : React.createElement('div', { className: 'text-rose-600' }, 'Define a top-level: function Canvas() { return (...) }');
      ReactDOM.createRoot(document.getElementById('root')).render(element);
    } catch (e) {
      fail('run: ' + String((e && e.stack) || e));
    }
    // Report the post-render error state (empty list means success).
    window.__reportCanvasErrors();
  })();
</script>

<script>
  // On-canvas diagnostic: if nothing rendered, report what loaded (so we can
  // debug without the web inspector).
  setTimeout(function () {
    var root = document.getElementById('root');
    if (root && root.childElementCount === 0) {
      root.innerHTML = '<pre style="white-space:pre-wrap;color:#c0392b;font:13px ui-monospace,monospace;padding:1rem">'
        + 'Canvas runtime diagnostic (nothing rendered)\n\n'
        + 'React: ' + (typeof React) + '\n'
        + 'ReactDOM: ' + (typeof ReactDOM) + '\n'
        + 'Babel: ' + (typeof Babel) + '\n'
        + 'tailwind: ' + (typeof tailwind) + '\n\n'
        + 'Errors:\n' + ((window.__canvasErrors && window.__canvasErrors.length) ? window.__canvasErrors.join('\n') : '(none captured)')
        + '</pre>';
    }
  }, 2000);
</script>
</body>
</html>
"####;

/// `tsconfig.json` written into `~/.agents/canvases/` so the TS language server
/// lints `.canvas.tsx` files (no npm install needed).
const TSCONFIG_JSON: &str = r##"{
  "compilerOptions": {
    "target": "ES2020",
    "lib": ["ES2020", "DOM", "DOM.Iterable"],
    "jsx": "react",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "noEmit": true,
    "allowJs": true,
    "checkJs": false,
    "strict": false,
    "skipLibCheck": true,
    "types": []
  },
  "include": ["*.tsx", "*.d.ts"]
}
"##;

/// Ambient declarations for the canvas SDK, written into
/// `~/.agents/canvases/canvas.d.ts`. This is what gives `.canvas.tsx` files
/// autocomplete + type errors for the global components (no imports needed).
/// Keep in sync with the SDK defined in `CANVAS_SHELL`.
const CANVAS_DTS: &str = r##"// Generated by Zed's canvas feature. Declares the globals available to
// .canvas.tsx files — do not import anything; these are all global.

export {};

declare global {
  const React: {
    createElement(type: any, props?: any, ...children: any[]): any;
    Fragment: any;
    useState<S>(initial: S | (() => S)): [S, (value: S | ((prev: S) => S)) => void];
    useEffect(effect: () => void | (() => void), deps?: any[]): void;
    useMemo<T>(factory: () => T, deps?: any[]): T;
    useRef<T>(initial: T): { current: T };
  };

  namespace JSX {
    interface IntrinsicElements {
      [elem: string]: any;
    }
    type Element = any;
  }

  interface HostTheme {
    kind: "light" | "dark";
    name: string | null;
    colors: {
      background: string;
      surface: string;
      panel: string;
      element: string;
      border: string;
      text: string;
      textMuted: string;
      accent: string;
      error: string;
      warning: string;
      success: string;
      info: string;
      [key: string]: string;
    };
  }
  function useHostTheme(): HostTheme;

  interface PageProps { children?: any; prose?: boolean; className?: string }
  function Page(props: PageProps): JSX.Element;

  interface FlexProps { children?: any; gap?: number; className?: string }
  function Stack(props: FlexProps): JSX.Element;
  function Row(props: FlexProps): JSX.Element;

  interface GridProps { children?: any; cols?: number; gap?: number; className?: string }
  function Grid(props: GridProps): JSX.Element;

  interface CardProps { title?: string; children?: any; className?: string }
  function Card(props: CardProps): JSX.Element;

  interface StatProps { label?: any; value?: any; delta?: any; tone?: "up" | "down" }
  function Stat(props: StatProps): JSX.Element;

  interface ButtonProps { children?: any; onClick?: () => void; variant?: "primary" }
  function Button(props: ButtonProps): JSX.Element;

  interface BadgeProps { children?: any; tone?: "success" | "danger" }
  function Badge(props: BadgeProps): JSX.Element;

  interface TableProps { headers?: any[]; rows?: any[][]; align?: ("left" | "right")[] }
  function Table(props: TableProps): JSX.Element;

  interface BarChartProps { data?: { label: string; value: number }[]; height?: number; accent?: string }
  function BarChart(props: BarChartProps): JSX.Element;

  function cx(...args: any[]): string;
}
"##;
