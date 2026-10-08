//! Window-bound session loads (ADR 0112 §9, §12): every runtime load, reopen,
//! refresh and read view adopts the session's current frame, never its whole
//! history.
use super::{SessionStore, SessionWindowRead, StoreError, WindowSelector, validate_window_session};

/// A session's current frame adopted as runtime state, with the config the
/// window read carried.
#[derive(Debug)]
pub struct LoadedSessionWindow {
    pub state: crate::RuntimeSessionState,
    pub config: crate::PersistedSessionConfig,
}

/// Adopt a window read onto a default state. Adoption is head-authoritative
/// (FIG-1875): every config fact is the head's, and the window names its
/// session.
pub fn window_state(
    read: SessionWindowRead,
    fleet: super::FleetFormat,
) -> Result<LoadedSessionWindow, StoreError> {
    let config = read.config.clone();
    let mut state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
        config.turn_budget,
        config.max_tool_calls,
        crate::NoProgressBudget::bounded(12),
    ));
    crate::runtime::state::adopt_durable_head(&mut state, read, fleet)?;
    Ok(LoadedSessionWindow { state, config })
}

/// Load the view's session at `selector` as runtime state, after checking
/// that this build can read its session-state version.
///
/// `Ok(None)` means the session has no head row (`Current` only).
pub async fn load_session_window_state(
    store: &SessionStore,
    selector: WindowSelector,
) -> Result<Option<LoadedSessionWindow>, StoreError> {
    store.read_session_state_version().await?;
    let Some(read) = store.load_session_window(selector).await? else {
        return Ok(None);
    };
    validate_window_session(store.session_id(), &read)?;
    window_state(read, store.fleet_format()).map(Some)
}

/// The canonical read-only view of the session's current frame, together
/// with its durable session relation.
///
/// Failure evidence is not part of the view; it is paged through
/// [`SessionHistoryStore::load_failure_evidence_page`](super::SessionHistoryStore::load_failure_evidence_page).
pub async fn load_session_read_view(
    store: &SessionStore,
) -> Result<Option<crate::SessionReadView>, StoreError> {
    let Some(read) = store.load_session_window(WindowSelector::Current).await? else {
        return Ok(None);
    };
    validate_window_session(store.session_id(), &read)?;
    let meta = store.load_session_meta().await?.ok_or_else(|| {
        StoreError::Backend(format!(
            "session `{}` has durable head state but no session metadata",
            read.session_id
        ))
    })?;
    let loaded = window_state(read, store.fleet_format())?;
    Ok(Some(
        crate::SessionReadView::from_persisted_state_with_relation(&loaded.state, meta.relation),
    ))
}

/// Re-adopt the session's current frame into `state`. All of it, the
/// execution controls included (FIG-4376), is the head's. Answers whether
/// the session has a head: one with none leaves `state` unchanged.
pub async fn refresh_session_window(
    store: &SessionStore,
    state: &mut crate::RuntimeSessionState,
) -> Result<bool, StoreError> {
    let Some(read) = store.load_session_window(WindowSelector::Current).await? else {
        return Ok(false);
    };
    validate_window_session(store.session_id(), &read)?;
    *state = window_state(read, store.fleet_format())?.state;
    Ok(true)
}
