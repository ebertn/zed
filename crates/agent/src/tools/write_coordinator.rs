use async_lock::Mutex;
use collections::HashMap;
use gpui::{App, EntityId, Global};
use std::sync::Arc;

/// Coordinates concurrent file edits across all agents in the app (the primary
/// agent and any background subagents), so they never apply edits to the same
/// buffer at the same time.
///
/// Each buffer gets its own async mutex. An [`EditSession`](super::edit_session::EditSession)
/// holds the buffer's lock for its entire lifetime, so:
/// - concurrent edit sessions targeting the **same** buffer serialize (the
///   second waits for the first to finish, then re-resolves its edits against
///   the now-current buffer content — and fails loudly if its `old_text` no
///   longer matches, rather than applying at a stale offset), while
/// - sessions on **different** buffers proceed in parallel.
///
/// Stored as an app global so a single instance is shared across every agent
/// thread in the app (parent and subagents alike).
#[derive(Default)]
pub(crate) struct WriteCoordinator {
    buffer_locks: HashMap<EntityId, Arc<Mutex<()>>>,
}

impl Global for WriteCoordinator {}

impl WriteCoordinator {
    /// Returns the lock guarding edits to the given buffer, creating it on first
    /// use. Acquire it with [`Mutex::lock_arc`] and hold the guard for the
    /// duration of the edit.
    pub(crate) fn buffer_lock(buffer_id: EntityId, cx: &mut App) -> Arc<Mutex<()>> {
        cx.default_global::<WriteCoordinator>()
            .buffer_locks
            .entry(buffer_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    async fn test_buffer_lock_serializes_same_buffer(cx: &mut TestAppContext) {
        let (id_a, id_b) =
            cx.update(|cx| (cx.new(|_| 0u8).entity_id(), cx.new(|_| 0u8).entity_id()));

        let lock_a1 = cx.update(|cx| WriteCoordinator::buffer_lock(id_a, cx));
        let lock_a2 = cx.update(|cx| WriteCoordinator::buffer_lock(id_a, cx));
        let lock_b = cx.update(|cx| WriteCoordinator::buffer_lock(id_b, cx));

        assert!(
            Arc::ptr_eq(&lock_a1, &lock_a2),
            "the same buffer must map to the same lock"
        );
        assert!(
            !Arc::ptr_eq(&lock_a1, &lock_b),
            "different buffers must map to different locks"
        );

        let guard = lock_a1.lock_arc().await;
        assert!(
            lock_a2.try_lock_arc().is_none(),
            "a second edit of the same buffer must wait"
        );
        assert!(
            lock_b.try_lock_arc().is_some(),
            "a different buffer can be edited in parallel"
        );

        drop(guard);
        assert!(
            lock_a2.try_lock_arc().is_some(),
            "the lock must be released when the session ends"
        );
    }
}
