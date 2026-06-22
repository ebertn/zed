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

use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use gpui::{App, Context, Global, Window};
use workspace::Workspace;

/// Identifies an open surface instance. Scoped to the provider that created it
/// (i.e. unique per `kind`, not globally).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub u64);

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
