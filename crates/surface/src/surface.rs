//! A small registry for "surfaces": modular, addressable pane contents that
//! something else (a user action, an agent tool, ...) can open and update by a
//! stable string `kind`.
//!
//! This is the generic "window pane type system" the canvas WebView plugs into.
//! A provider knows how to open a workspace `Item` of its kind and update an
//! existing instance. Providers register themselves at startup; callers open and
//! update surfaces without knowing the concrete view type.
//!
//! Intentionally minimal and personal-scoped: macOS-only consumers, no
//! persistence, no non-macOS stubs beyond what the type system needs.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow};
use gpui::{App, Context, Global, SharedString, Window};
use serde::{Deserialize, Serialize};
use workspace::Workspace;

/// Identifies an open surface instance. Scoped to the provider that created it
/// (i.e. unique per `kind`, not globally).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub u64);

/// An opaque key associating surfaces with the conversation that created them
/// (in practice an agent session id). Kept dependency-free so this crate stays
/// lean: callers stringify whatever identity they have.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey(Arc<str>);

impl SessionKey {
    pub fn new(key: impl Into<Arc<str>>) -> Self {
        Self(key.into())
    }
}

impl From<&str> for SessionKey {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl From<String> for SessionKey {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

/// Opens and updates one kind of surface (e.g. a WebView canvas).
pub trait SurfaceProvider: 'static {
    /// Stable identifier for this kind of surface, e.g. `"canvas"`.
    fn kind(&self) -> &'static str;

    /// Opens a new instance of this surface in `workspace`, returning an id that
    /// can later be passed to [`SurfaceProvider::update`]. `params` is provider
    /// defined (JSON so agent tools can produce it directly).
    fn open(
        &self,
        params: serde_json::Value,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<SurfaceId>;

    /// Updates an existing instance with new `params`.
    fn update(&self, id: SurfaceId, params: serde_json::Value, cx: &mut App) -> Result<()>;

    /// Brings an already-open instance to the foreground, returning whether it
    /// was still live. The default returns `false`, meaning "no live instance"
    /// so the caller falls back to [`SurfaceProvider::open`].
    fn focus(
        &self,
        _id: SurfaceId,
        _workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> Result<bool> {
        Ok(false)
    }
}

#[derive(Default)]
struct SurfaceRegistry {
    providers: HashMap<&'static str, Arc<dyn SurfaceProvider>>,
}

impl Global for SurfaceRegistry {}

/// Registers a provider for its `kind`. Replaces any existing provider with the
/// same kind. Safe to call before any other surface API (initializes the
/// registry on first use).
pub fn register_surface_provider(cx: &mut App, provider: Arc<dyn SurfaceProvider>) {
    let kind = provider.kind();
    if !cx.has_global::<SurfaceRegistry>() {
        cx.set_global(SurfaceRegistry::default());
    }
    cx.global_mut::<SurfaceRegistry>()
        .providers
        .insert(kind, provider);
}

/// Opens a surface of the given `kind` in `workspace`.
pub fn open_surface(
    kind: &str,
    params: serde_json::Value,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<SurfaceId> {
    let provider = provider_for(kind, cx)?;
    provider.open(params, workspace, window, cx)
}

/// Updates an existing surface instance of the given `kind`.
pub fn update_surface(
    kind: &str,
    id: SurfaceId,
    params: serde_json::Value,
    cx: &mut App,
) -> Result<()> {
    let provider = provider_for(kind, cx)?;
    provider.update(id, params, cx)
}

fn provider_for(kind: &str, cx: &App) -> Result<Arc<dyn SurfaceProvider>> {
    cx.try_global::<SurfaceRegistry>()
        .and_then(|registry| registry.providers.get(kind).cloned())
        .ok_or_else(|| anyhow!("no surface provider registered for kind {kind:?}"))
}

// --- Session → surface association (persisted) --------------------------------
//
// Records which surfaces a conversation created, so a UI can offer to (re)open
// them. Persisted to disk so the association survives restarts; on reopen the
// stored `params` are replayed through the provider's `open`.

/// A surface created within a session, as surfaced to UIs.
#[derive(Clone, Debug)]
pub struct SurfaceInfo {
    /// Index of this surface within its session, used to address it in
    /// [`focus_surface`].
    pub index: usize,
    pub kind: SharedString,
    pub title: SharedString,
}

#[derive(Clone)]
struct SurfaceEntry {
    kind: SharedString,
    title: SharedString,
    /// Provider-defined params sufficient to (re)open the surface.
    params: serde_json::Value,
    /// The live instance id, if currently open this session.
    live_id: Option<SurfaceId>,
}

#[derive(Serialize, Deserialize)]
struct PersistedSurface {
    kind: String,
    title: String,
    params: serde_json::Value,
}

#[derive(Default)]
struct SessionSurfaces {
    sessions: HashMap<SessionKey, Vec<SurfaceEntry>>,
    loaded: bool,
}

impl Global for SessionSurfaces {}

fn surfaces_path() -> PathBuf {
    paths::data_dir().join("surfaces.json")
}

/// Loads the persisted session→surface map into the global, once. Safe to call
/// repeatedly; only the first call reads from disk. Call at startup so the map
/// is available to read-only render paths.
pub fn load_persisted(cx: &mut App) {
    if !cx.has_global::<SessionSurfaces>() {
        cx.set_global(SessionSurfaces::default());
    }
    if cx.global::<SessionSurfaces>().loaded {
        return;
    }
    let sessions = read_persisted();
    let store = cx.global_mut::<SessionSurfaces>();
    store.sessions = sessions;
    store.loaded = true;
}

fn read_persisted() -> HashMap<SessionKey, Vec<SurfaceEntry>> {
    let path = surfaces_path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let parsed: HashMap<String, Vec<PersistedSurface>> = match serde_json::from_str(&data) {
        Ok(parsed) => parsed,
        Err(err) => {
            log::error!("surface: failed to parse {}: {err}", path.display());
            return HashMap::new();
        }
    };
    parsed
        .into_iter()
        .map(|(key, entries)| {
            let entries = entries
                .into_iter()
                .map(|entry| SurfaceEntry {
                    kind: entry.kind.into(),
                    title: entry.title.into(),
                    params: entry.params,
                    live_id: None,
                })
                .collect();
            (SessionKey::from(key), entries)
        })
        .collect()
}

fn write_persisted(store: &SessionSurfaces) {
    let serializable: HashMap<&str, Vec<PersistedSurface>> = store
        .sessions
        .iter()
        .map(|(key, entries)| {
            let entries = entries
                .iter()
                .map(|entry| PersistedSurface {
                    kind: entry.kind.to_string(),
                    title: entry.title.to_string(),
                    params: entry.params.clone(),
                })
                .collect();
            (key.0.as_ref(), entries)
        })
        .collect();
    let path = surfaces_path();
    let json = match serde_json::to_string_pretty(&serializable) {
        Ok(json) => json,
        Err(err) => {
            log::error!("surface: failed to serialize surfaces: {err}");
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        log::error!("surface: failed to create {}: {err}", parent.display());
        return;
    }
    if let Err(err) = std::fs::write(&path, json) {
        log::error!("surface: failed to write {}: {err}", path.display());
    }
}

/// Records that `session` created a surface, so a UI can later (re)open it. If a
/// surface with the same `kind` and `title` already exists for the session, it
/// is updated in place (canvases are keyed by title on disk, so re-creating one
/// shouldn't duplicate the entry). Persists the change.
pub fn record_session_surface(
    session: SessionKey,
    kind: &'static str,
    title: impl Into<SharedString>,
    params: serde_json::Value,
    live_id: SurfaceId,
    cx: &mut App,
) {
    load_persisted(cx);
    let title = title.into();
    let store = cx.global_mut::<SessionSurfaces>();
    let entries = store.sessions.entry(session).or_default();
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.kind == kind && entry.title == title)
    {
        existing.params = params;
        existing.live_id = Some(live_id);
    } else {
        entries.push(SurfaceEntry {
            kind: kind.into(),
            title,
            params,
            live_id: Some(live_id),
        });
    }
    write_persisted(store);
}

/// The surfaces created in `session`, in creation order.
pub fn session_surfaces(session: &SessionKey, cx: &App) -> Vec<SurfaceInfo> {
    let Some(store) = cx.try_global::<SessionSurfaces>() else {
        return Vec::new();
    };
    store
        .sessions
        .get(session)
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| SurfaceInfo {
                    index,
                    kind: entry.kind.clone(),
                    title: entry.title.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Brings the `index`-th surface of `session` to the foreground, reopening it
/// from its persisted params if the live instance is gone.
pub fn focus_surface(
    session: &SessionKey,
    index: usize,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<()> {
    let (kind, params, live_id) = {
        let store = cx
            .try_global::<SessionSurfaces>()
            .ok_or_else(|| anyhow!("no surfaces recorded"))?;
        let entry = store
            .sessions
            .get(session)
            .and_then(|entries| entries.get(index))
            .ok_or_else(|| anyhow!("no surface at index {index}"))?;
        (entry.kind.clone(), entry.params.clone(), entry.live_id)
    };

    let provider = provider_for(&kind, cx)?;
    if let Some(id) = live_id
        && provider.focus(id, workspace, window, cx)?
    {
        return Ok(());
    }

    let new_id = provider.open(params, workspace, window, cx)?;
    if cx.has_global::<SessionSurfaces>()
        && let Some(entry) = cx
            .global_mut::<SessionSurfaces>()
            .sessions
            .get_mut(session)
            .and_then(|entries| entries.get_mut(index))
    {
        entry.live_id = Some(new_id);
    }
    Ok(())
}
