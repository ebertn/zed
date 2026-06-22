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
    canvas, div, point, px, size,
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

pub fn init(cx: &mut App) {
    surface::register_surface_provider(cx, Arc::new(CanvasProvider));
    surface::load_persisted(cx);

    cx.observe_new(|_workspace: &mut Workspace, window, cx| {
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
        thread.add_tool(CreateCanvasTool {
            session_id: thread.id().clone(),
        });
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

    // `open` calls this while still holding `&mut Window` for this window, so the
    // window is checked out of `cx` and a synchronous `window.update` here fails
    // with "window not found" (leaving the surface opaque, which hides the
    // WebView behind the transparency hole). Defer so the refresh runs once the
    // window borrow has been released and the handle resolves again.
    cx.defer(move |cx| refresh_window_transparency(window, cx));
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
        // `set_background_appearance` flips the Metal layer's opacity but does not
        // redraw. Without a redraw the previously-drawn (opaque) frame stays on
        // screen, so the transparency hole reads black until some other change
        // happens to dirty the window. Schedule a redraw so the change is visible
        // immediately.
        window.refresh();
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
    // Disable the eslint language server for this scaffold: it auto-starts for
    // `.tsx` files but there is no ESLint installed here (no `node_modules`), so
    // it only emits a noisy "eslint/noLibrary" failure. Canvas type support comes
    // from vtsls + the scaffolded tsconfig, so eslint adds nothing.
    let zed_dir = dir.join(".zed");
    if let Err(err) = std::fs::create_dir_all(&zed_dir) {
        log::error!("canvas: failed to create {}: {err}", zed_dir.display());
        return;
    }
    let settings = zed_dir.join("settings.json");
    if !settings.exists()
        && let Err(err) = std::fs::write(&settings, CANVAS_ZED_SETTINGS)
    {
        log::error!("canvas: failed to write .zed/settings.json: {err}");
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





fn try_global_mut<G: Global>(cx: &mut App) -> Option<&mut G> {
    cx.has_global::<G>().then(|| cx.global_mut::<G>())
}

// --- Agent tools -------------------------------------------------------------

/// Creates a canvas — a panel that renders a React component to present
/// information to the user (reports, dashboards, summaries, charts) — and opens
/// it already populated with `content`, so it never flashes empty. Use this for
/// any new canvas. After it opens, call `canvas_errors` (with the returned id)
/// to confirm it rendered; if it reports errors, fix them by editing the file
/// with `edit_file` (it hot-reloads).
///
/// `content` must define exactly one top-level `function Canvas() { ... }`
/// returning the UI. IMPORTANT RULES for its contents:
/// - Do NOT use `import` or `export`, and do NOT write TypeScript type
///   annotations. React and the component library are provided as globals and
///   only JSX is transpiled.
///
/// Components available as globals (no import needed):
/// - Layout/typography: `<Page>…</Page>` (centered, readable `prose`; put
///   h1/h2/p/ul/blockquote/code directly inside), `<Grid cols={n}>`, `<Stack>`,
///   `<Row>`.
/// - shadcn/ui: `<Button variant="default|secondary|outline|destructive|ghost|link">`,
///   `<Card>`/`<CardHeader>`/`<CardTitle>`/`<CardDescription>`/`<CardContent>`/`<CardFooter>`,
///   `<Badge variant="default|secondary|destructive|outline">`,
///   `<Table>`/`<TableHeader>`/`<TableBody>`/`<TableRow>`/`<TableHead>`/`<TableCell>`,
///   `<Alert>`/`<AlertTitle>`/`<AlertDescription>`, `<Separator>`,
///   `<Tabs>`/`<TabsList>`/`<TabsTrigger>`/`<TabsContent>`.
/// - Charts (Recharts, exposed as globals): `<ResponsiveContainer>`, `<BarChart>`,
///   `<Bar>`, `<LineChart>`, `<Line>`, `<AreaChart>`, `<Area>`, `<PieChart>`,
///   `<Pie>`, `<Cell>`, `<XAxis>`, `<YAxis>`, `<CartesianGrid>`, `<Tooltip>`,
///   `<Legend>` (also the full `Recharts` namespace). Wrap a chart in a sized
///   `<div style={{ width: "100%", height: 240 }}>` with `<ResponsiveContainer>`.
///   Color series with the theme palette so charts match the rest of the UI:
///   use `fill`/`stroke="hsl(var(--chart-1))"` (matches the primary/button color)
///   through `hsl(var(--chart-5))`, and `hsl(var(--border))`/
///   `hsl(var(--muted-foreground))` for grid/axes. Do NOT hardcode hex colors.
/// - Icons (lucide): e.g. `<TrendingUp />`, `<Check />`, `<Info />` (also the
///   `Icons` namespace).
/// - `useHostTheme()` -> `{ kind: "light" | "dark", name: string | null, colors: Record<string, string> }`.
/// - `cn(...classes)` to compose Tailwind classes.
///
/// Style with shadcn/Tailwind token classes (`bg-card`, `text-muted-foreground`,
/// `bg-primary`, `border`, etc.); they automatically follow the Zed theme.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CreateCanvasToolInput {
    /// Short title shown on the canvas tab. Also identifies the canvas: a later
    /// `create_canvas` or `canvas_open` with the same title targets the same file.
    title: String,
    /// The full canvas source: exactly one top-level `function Canvas() { ... }`
    /// returning the UI (no `import`/`export`, no TypeScript type annotations).
    content: String,
}

struct CreateCanvasTool {
    /// The session that owns this tool instance, so the created canvas can be
    /// associated with (and reopened from) the conversation.
    session_id: acp::SessionId,
}

impl AgentTool for CreateCanvasTool {
    type Input = CreateCanvasToolInput;
    type Output = String;

    const NAME: &'static str = "create_canvas";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Create canvas: {}", input.title).into(),
            Err(_) => "Create canvas".into(),
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
            if input.content.trim().is_empty() {
                return Err("create_canvas requires non-empty `content` defining `function Canvas() { ... }`".to_string());
            }
            // Write the provided content before opening so the canvas renders
            // populated from the first frame, rather than flashing the empty
            // starter and being filled in afterward.
            scaffold_canvas_project();
            let path = canvases_dir().join(format!("{}.canvas.tsx", slugify(&input.title)));
            std::fs::write(&path, &input.content)
                .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
            // Make the canvases dir a (non-visible) worktree so `edit_file` and the
            // language server work on the file for follow-up fixes.
            ensure_canvas_worktree(cx).await.map_err(|err| err.to_string())?;
            let path_string = path.to_string_lossy().to_string();
            // A fresh open loads the new content from disk; an already-open canvas
            // with this title must be reloaded explicitly to pick it up.
            let already_open =
                cx.update(|cx| live_canvas_id_for_path(&path, cx).is_some());
            let id = cx
                .update(|cx| open_or_focus_canvas(&input.title, &path, cx))
                .map_err(|err| err.to_string())?;
            let title = input.title.clone();
            let content = input.content.clone();
            cx.update(|cx| {
                if already_open
                    && let Some(view) = canvas_by_id(id.0, cx)
                {
                    view.update(cx, |view, cx| view.set_content(&content, cx));
                }
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
                "Created and opened canvas \"{}\" (id {}) at `{}`, rendering your content. Call `canvas_errors` with id {} to confirm it rendered without errors; fix any reported errors by editing the file with `edit_file` (it hot-reloads).",
                input.title,
                id.0,
                path.display(),
                id.0
            ))
        })
    }
}

/// Reopens an existing canvas (one created earlier with `create_canvas`) by
/// `title` — focusing its tab if it's already open, otherwise opening it from
/// its file. For a brand-new canvas use `create_canvas` instead, so it renders
/// populated rather than flashing an empty starter. Returns the canvas id; use
/// it with `canvas_errors` to confirm the canvas rendered.
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

        // The Canvas/Code toggle floats over the top-right of the content. Its
        // rect is excluded from the mouse-passthrough region (see
        // `render_canvas_content`) so it stays clickable even though the rest of
        // the canvas passes events through to the WebView.
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .child(div().size_full().child(content))
            .child(self.render_floating_toggle(cx))
    }
}

impl CanvasView {
    /// The rendered-canvas content: a transparency hole that the native WebView
    /// shows through. The `canvas()` painter keeps the WebView aligned with the
    /// hole and registers the hole as a native mouse-passthrough region (minus
    /// the floating toggle's rect) so the WebView receives scroll/selection/
    /// click events while the toggle stays clickable.
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
                            // Keep the floating toggle (top-right) clickable by
                            // excluding a fixed region around it from the
                            // passthrough area; everything else reaches the
                            // WebView. The region is generous enough to cover the
                            // toggle at any reasonable theme/font size.
                            let exclude = Bounds {
                                origin: point(
                                    bounds.origin.x + bounds.size.width - px(TOGGLE_REGION_WIDTH),
                                    bounds.origin.y,
                                ),
                                size: size(px(TOGGLE_REGION_WIDTH), px(TOGGLE_REGION_HEIGHT)),
                            };
                            let rects = passthrough_rects(bounds, Some(exclude));
                            window.set_mouse_passthrough_rects(rects);
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

    /// The Canvas/Code toggle, floating over the top-right of the content.
    fn render_floating_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .absolute()
            .top_2()
            .right_2()
            .rounded_md()
            .bg(cx.theme().colors().elevated_surface_background)
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

/// Size of the top-right region reserved (excluded from mouse passthrough) for
/// the floating Canvas/Code toggle, in logical pixels. Generous enough to cover
/// the toggle at any reasonable theme/font size.
#[cfg(target_os = "macos")]
const TOGGLE_REGION_WIDTH: f32 = 200.0;
#[cfg(target_os = "macos")]
const TOGGLE_REGION_HEIGHT: f32 = 56.0;

/// The mouse-passthrough rectangles covering `content` minus `exclude` (the
/// floating toggle), so the WebView receives events everywhere except where the
/// toggle sits. Decomposes `content - exclude` into up to four non-overlapping
/// bands; returns the whole `content` when there's nothing to exclude.
#[cfg(target_os = "macos")]
fn passthrough_rects(
    content: Bounds<Pixels>,
    exclude: Option<Bounds<Pixels>>,
) -> Vec<Bounds<Pixels>> {
    let Some(exclude) = exclude else {
        return vec![content];
    };
    let c_left = f32::from(content.origin.x);
    let c_top = f32::from(content.origin.y);
    let c_right = c_left + f32::from(content.size.width);
    let c_bottom = c_top + f32::from(content.size.height);

    let ex_left = f32::from(exclude.origin.x).max(c_left);
    let ex_top = f32::from(exclude.origin.y).max(c_top);
    let ex_right = (f32::from(exclude.origin.x) + f32::from(exclude.size.width)).min(c_right);
    let ex_bottom = (f32::from(exclude.origin.y) + f32::from(exclude.size.height)).min(c_bottom);

    if ex_right <= ex_left || ex_bottom <= ex_top {
        return vec![content];
    }

    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Bounds {
        origin: point(px(x0), px(y0)),
        size: size(px(x1 - x0), px(y1 - y0)),
    };
    let mut rects = Vec::new();
    if ex_top > c_top {
        rects.push(rect(c_left, c_top, c_right, ex_top));
    }
    if ex_bottom < c_bottom {
        rects.push(rect(c_left, ex_bottom, c_right, c_bottom));
    }
    if ex_left > c_left {
        rects.push(rect(c_left, ex_top, ex_left, ex_bottom));
    }
    if ex_right < c_right {
        rects.push(rect(ex_right, ex_top, c_right, ex_bottom));
    }
    rects
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

    // Serve the document (and the embedded SDK bundle) from a custom scheme so
    // the page has a real origin. A `load_html` page is opaque-origin, which
    // scrubs script errors to bare "Script error." and breaks the runtime.
    let protocol_document = document;
    let protocol = move |request: wry::http::Request<Vec<u8>>| {
        let asset: Option<(&str, &str)> = match request.uri().path() {
            "/sdk.js" => Some(("text/javascript", CANVAS_SDK_JS)),
            "/tailwind.js" => Some(("text/javascript", CANVAS_TAILWIND_JS)),
            "/babel.js" => Some(("text/javascript", CANVAS_BABEL_JS)),
            _ => None,
        };
        if let Some((content_type, body)) = asset {
            return Response::builder()
                .header(CONTENT_TYPE, content_type)
                .body(Cow::Borrowed(body.as_bytes()))
                .unwrap_or_else(|_| Response::new(Cow::Borrowed(&b""[..])));
        }
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
        // The canvas is a display surface: it should receive mouse events (scroll,
        // text selection) but never steal keyboard focus from Zed. wry focuses the
        // WebView on creation by default, which makes its content view the window's
        // first responder once the page loads, so keystrokes go to the WebView and
        // Zed beeps. Opt out so keyboard focus stays with GPUI.
        .with_focused(false)
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

    // Derive a visibly-distinct "subtle surface" for shadcn's secondary/muted/
    // accent tokens by nudging the background's lightness toward the foreground
    // (lighter in dark themes, darker in light themes). Mapping these straight to
    // a Zed element color tends to vanish into the background.
    let dark = !theme.appearance().is_light();
    let elevate = |base: gpui::Hsla, amount: f32| gpui::Hsla {
        l: (base.l + amount).clamp(0.0, 1.0),
        ..base
    };
    let subtle_surface = elevate(colors.background, if dark { 0.10 } else { -0.06 });
    // A chart palette anchored on the accent so the first series matches the
    // primary (button) color; later series rotate hue for distinction.
    let accent = colors.text_accent;
    let chart = |steps: f32| gpui::Hsla {
        h: (accent.h + steps * 0.11).rem_euclid(1.0),
        ..accent
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
        },
        // shadcn token theme as HSL triplets ("H S% L%"), consumed as
        // `hsl(var(--name))` by the bundled shadcn components.
        "vars": {
            "background": hsl_triplet(colors.background),
            "foreground": hsl_triplet(colors.text),
            "card": hsl_triplet(colors.elevated_surface_background),
            "card-foreground": hsl_triplet(colors.text),
            "popover": hsl_triplet(colors.elevated_surface_background),
            "popover-foreground": hsl_triplet(colors.text),
            "primary": hsl_triplet(colors.text_accent),
            "primary-foreground": hsl_triplet(colors.background),
            "secondary": hsl_triplet(subtle_surface),
            "secondary-foreground": hsl_triplet(colors.text),
            "muted": hsl_triplet(subtle_surface),
            "muted-foreground": hsl_triplet(colors.text_muted),
            "accent": hsl_triplet(subtle_surface),
            "accent-foreground": hsl_triplet(colors.text),
            "destructive": hsl_triplet(status.error),
            "destructive-foreground": hsl_triplet(colors.background),
            "border": hsl_triplet(colors.border),
            "input": hsl_triplet(colors.border),
            "ring": hsl_triplet(colors.text_accent),
            "chart-1": hsl_triplet(chart(0.0)),
            "chart-2": hsl_triplet(chart(1.0)),
            "chart-3": hsl_triplet(chart(2.0)),
            "chart-4": hsl_triplet(chart(3.0)),
            "chart-5": hsl_triplet(chart(4.0)),
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

/// Formats a GPUI color as a shadcn-style HSL triplet, e.g. `"220 13% 18%"`,
/// for use as `hsl(var(--token))`.
fn hsl_triplet(color: gpui::Hsla) -> String {
    let h = (color.h.clamp(0.0, 1.0) * 360.0).round();
    let s = (color.s.clamp(0.0, 1.0) * 100.0).round();
    let l = (color.l.clamp(0.0, 1.0) * 100.0).round();
    format!("{h} {s}% {l}%")
}

/// The esbuild-built canvas SDK bundle (React + ReactDOM + the component
/// library), served to the page at `zedcanvas://localhost/sdk.js`. Rebuild it
/// with `npm run build` in `crates/web_canvas/sdk` after editing the SDK source.
#[cfg(target_os = "macos")]
const CANVAS_SDK_JS: &str = include_str!("../assets/canvas-sdk.js");

/// Vendored Tailwind (Play CDN JIT engine) served at `zedcanvas://localhost/tailwind.js`,
/// so canvases style themselves offline. Re-download with the `curl` in the SDK
/// README if bumping versions.
#[cfg(target_os = "macos")]
const CANVAS_TAILWIND_JS: &str = include_str!("../assets/tailwind.js");

/// Vendored `@babel/standalone` served at `zedcanvas://localhost/babel.js`, used
/// to transpile the agent's JSX in-browser. (Stage 2 will move transpilation
/// host-side and drop this.)
#[cfg(target_os = "macos")]
const CANVAS_BABEL_JS: &str = include_str!("../assets/babel.min.js");

/// React + Tailwind (+ typography) + Babel runtime shell. `CANVAS_TITLE_PLACEHOLDER`
/// and `// CANVAS_BODY_PLACEHOLDER` are substituted by [`canvas_document`]. The
/// React runtime and component library come from the embedded SDK bundle
/// (`/sdk.js`); the user's canvas is transpiled in-browser by Babel and mounted
/// as `<Canvas/>`.
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
    var root = document.documentElement.style;
    var colors = t.colors || {};
    for (var key in colors) {
      if (Object.prototype.hasOwnProperty.call(colors, key)) {
        root.setProperty('--zed-' + key, colors[key]);
      }
    }
    // shadcn token variables (HSL triplets like "220 13% 18%").
    var vars = t.vars || {};
    for (var name in vars) {
      if (Object.prototype.hasOwnProperty.call(vars, name)) {
        root.setProperty('--' + name, vars[name]);
      }
    }
  })();
</script>
<script src="zedcanvas://localhost/tailwind.js"></script>
<script>
  // Tailwind config wired to the shadcn CSS-variable token theme. The variables
  // themselves (`--background`, `--primary`, …) are set from the Zed theme by the
  // bootstrap above.
  tailwind.config = {
    darkMode: 'class',
    theme: {
      extend: {
        colors: {
          border: 'hsl(var(--border))',
          input: 'hsl(var(--input))',
          ring: 'hsl(var(--ring))',
          background: 'hsl(var(--background))',
          foreground: 'hsl(var(--foreground))',
          primary: { DEFAULT: 'hsl(var(--primary))', foreground: 'hsl(var(--primary-foreground))' },
          secondary: { DEFAULT: 'hsl(var(--secondary))', foreground: 'hsl(var(--secondary-foreground))' },
          destructive: { DEFAULT: 'hsl(var(--destructive))', foreground: 'hsl(var(--destructive-foreground))' },
          muted: { DEFAULT: 'hsl(var(--muted))', foreground: 'hsl(var(--muted-foreground))' },
          accent: { DEFAULT: 'hsl(var(--accent))', foreground: 'hsl(var(--accent-foreground))' },
          popover: { DEFAULT: 'hsl(var(--popover))', foreground: 'hsl(var(--popover-foreground))' },
          card: { DEFAULT: 'hsl(var(--card))', foreground: 'hsl(var(--card-foreground))' },
        },
        borderRadius: { lg: 'var(--radius)', md: 'calc(var(--radius) - 2px)', sm: 'calc(var(--radius) - 4px)' },
        fontFamily: { serif: ['ETBembo', '"Palatino Linotype"', 'Palatino', 'Georgia', 'serif'] },
      },
    },
  };
</script>
<script src="zedcanvas://localhost/sdk.js"></script>
<script src="zedcanvas://localhost/babel.js"></script>
<style>
  html, body { margin: 0; height: 100%; }
  :root { --radius: 0.5rem; }
  /* Default border color to the shadcn token (mirrors shadcn's base layer). */
  * { border-color: hsl(var(--border, 0 0% 85%)); }
  /* Driven by the host theme via the shadcn `--background`/`--foreground` tokens
     (set by the theme bootstrap), with light defaults as a fallback. */
  body { background: hsl(var(--background, 60 100% 99%)); color: hsl(var(--foreground, 0 0% 7%)); }
  /* Make code / equation blocks theme-aware even outside `prose` (e.g. inside a
     Card), so agent-authored blocks follow the host theme automatically. */
  pre, code, kbd, samp { background: var(--zed-element, rgba(0, 0, 0, 0.06)); border-radius: 4px; }
  #root { min-height: 100%; }
</style>
</head>
<body class="font-serif">
<div id="root" class="px-8 py-6"></div>

<script type="text/plain" id="canvas-body">
// CANVAS_BODY_PLACEHOLDER
</script>

<script>
  // Transpile the agent's canvas with Babel's stable transform API and run it via
  // indirect eval (global scope). The React runtime + component library come from
  // the bundled SDK (sdk.js), which already populated the globals. We avoid
  // Babel's transformScriptTags / dynamic <script> injection, which throws
  // (appendChild) inside WKWebView.
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
      if (typeof React === 'undefined' || typeof ReactDOM === 'undefined') { return fail('Canvas SDK (sdk.js) failed to load'); }
      var body = document.getElementById('canvas-body').textContent;
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

/// `.zed/settings.json` written into `~/.agents/canvases/` to disable the eslint
/// language server for canvas files. Zed auto-starts eslint for `.tsx`, but the
/// scaffold has no ESLint installed, so it only emits a noisy failure. `"..."`
/// keeps the remaining default servers (vtsls, tailwind) intact.
const CANVAS_ZED_SETTINGS: &str = r##"{
  "languages": {
    "TSX": { "language_servers": ["!eslint", "..."] },
    "TypeScript": { "language_servers": ["!eslint", "..."] }
  }
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
  function cn(...args: any[]): string;

  // Layout / typography helpers.
  function Page(props: any): JSX.Element;
  function Stack(props: any): JSX.Element;
  function Row(props: any): JSX.Element;
  function Grid(props: any): JSX.Element;

  // shadcn/ui components (props are loosely typed for authoring convenience).
  const Button: any;
  const Card: any;
  const CardHeader: any;
  const CardFooter: any;
  const CardTitle: any;
  const CardDescription: any;
  const CardContent: any;
  const Badge: any;
  const Table: any;
  const TableHeader: any;
  const TableBody: any;
  const TableFooter: any;
  const TableHead: any;
  const TableRow: any;
  const TableCell: any;
  const TableCaption: any;
  const Alert: any;
  const AlertTitle: any;
  const AlertDescription: any;
  const Separator: any;
  const Tabs: any;
  const TabsList: any;
  const TabsTrigger: any;
  const TabsContent: any;

  // Recharts (also available under the `Recharts` namespace).
  const Recharts: any;
  const ResponsiveContainer: any;
  const BarChart: any;
  const Bar: any;
  const LineChart: any;
  const Line: any;
  const AreaChart: any;
  const Area: any;
  const PieChart: any;
  const Pie: any;
  const Cell: any;
  const XAxis: any;
  const YAxis: any;
  const CartesianGrid: any;
  const Tooltip: any;
  const Legend: any;

  // lucide icons (also available under the `Icons` namespace).
  const Icons: any;
  const Activity: any;
  const AlertCircle: any;
  const AlertTriangle: any;
  const ArrowDownRight: any;
  const ArrowUpRight: any;
  const Check: any;
  const CheckCircle2: any;
  const ChevronRight: any;
  const CircleDot: any;
  const Info: any;
  const Minus: any;
  const Plus: any;
  const TrendingDown: any;
  const TrendingUp: any;
  const X: any;
  const XCircle: any;
}
"##;
