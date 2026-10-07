//! A session's committed head as a turn starts from it, and the head commit
//! a finished turn's `turn.commit` writes over it (ADR 0132 §4). Owned by L3
//! (FIG-5172).
//!
//! The head is read from the session store's current window, never from the
//! history before it, so what a turn loads does not grow with the turns the
//! session committed before its window.
//!
//! A turn starts from the head's window pinned by the head it is (FIG-5206):
//! its checkpoint names the window by that pin instead of holding it, and a
//! restore reads the window again at the pinned head, however far the live
//! head has moved since.

use std::sync::Arc;

use lash_sansio::TurnWindowPin;

use super::session::{TurnCommit, TurnDone, TurnError};
use crate::runtime::{RuntimeSessionState, TurnBoundary};
use crate::store::{SessionHeadRef, SessionStore, WindowSelector};
use crate::{Backend, Clock, CommitBudget, SessionId, TurnId, TurnOutcome};

/// The committed window a session's turn starts from.
pub type TurnWindow = lash_sansio::TurnWindow<crate::ProtocolEvent>;

/// A session's committed head.
#[derive(Clone)]
pub struct SessionHead {
    state: RuntimeSessionState,
    fleet: crate::FleetFormat,
    budget: CommitBudget,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for SessionHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHead")
            .field("session", &self.state.session_id)
            .field("revision", &self.state.head_revision)
            .finish_non_exhaustive()
    }
}

impl SessionHead {
    /// `session`'s head in `backend`'s session store, whose commits a turn
    /// writes under `budget`.
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the session is not in the store or has no
    /// head; the store's refusal.
    pub async fn load(
        backend: &Backend,
        session: &SessionId,
        budget: CommitBudget,
    ) -> Result<Self, TurnError> {
        let store = session_store(backend, session).await?;
        let state = load_state(&store, session, WindowSelector::Current).await?;
        Ok(Self {
            state,
            fleet: store.fleet_format(),
            budget,
            clock: backend.clock(),
        })
    }

    /// The head's revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.state.head_revision
    }

    /// The head's state.
    #[must_use]
    pub fn state(&self) -> &RuntimeSessionState {
        &self.state
    }

    /// The head's window, pinned by this head: what a turn that starts here
    /// starts from ([`TurnMachine::in_window`](crate::TurnMachine::in_window)).
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the pin does not encode.
    pub fn window(&self) -> Result<TurnWindow, TurnError> {
        let head = SessionHeadRef {
            generation: 0,
            revision: self.state.head_revision,
            leaf: self.state.session_graph.leaf_node_id.clone(),
            checkpoint: self.state.checkpoint_ref.clone(),
        };
        let pin = serde_json::to_string(&head).map_err(|error| {
            TurnError::Exec(format!("the window's pin does not encode: {error}"))
        })?;
        Ok(window_of(&self.state, TurnWindowPin::new(pin)))
    }

    /// The commit of `run` that publishes `done`'s messages and outcome as
    /// the head's next revision.
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the commit cannot be assembled.
    pub fn commit(&self, run: &TurnId, done: TurnDone) -> Result<TurnCommit, TurnError> {
        let terminal = done.terminal();
        let outcome = done
            .outcome
            .unwrap_or(TurnOutcome::Stopped(crate::TurnStop::Incomplete));
        let mut boundary = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.clock),
            crate::ExecutionScope::turn(self.state.session_id.clone(), run.clone()),
            self.budget,
        )
        .with_fleet_format(self.fleet);
        let commit = boundary
            .durable_commit(done.messages, &outcome, &[], None)
            .map_err(|error| TurnError::Exec(format!("the turn's head commit: {error}")))?;
        Ok(TurnCommit {
            expected_head: commit.expected_head_revision,
            commit_json: crate::store::encode_session_commit(&commit)
                .map_err(|error| TurnError::Exec(error.to_string()))?,
            terminal,
            cause_json: None,
        })
    }
}

/// `session`'s store in `backend`'s catalog.
pub(crate) async fn session_store(
    backend: &Backend,
    session: &SessionId,
) -> Result<SessionStore, TurnError> {
    let catalog = backend.session_store_factory();
    match catalog
        .lookup_session(session)
        .await
        .map_err(|error| TurnError::Exec(error.to_string()))?
    {
        crate::store::SessionLookup::Live(_) => {
            let runtime: Arc<dyn crate::store::RuntimeStore> = catalog;
            SessionStore::new(runtime, session.clone())
                .map_err(|error| TurnError::Exec(error.to_string()))
        }
        crate::store::SessionLookup::Deleted => {
            Err(TurnError::Exec(format!("session {session} was deleted")))
        }
        crate::store::SessionLookup::Absent => {
            Err(TurnError::Exec(format!("session {session} does not exist")))
        }
    }
}

/// The window `pin` names in `session`'s store ([`SessionHead::window`]),
/// read at the pinned head however far the live head has moved since.
/// Revision zero is the session before any head existed, whose window is
/// empty; any other is read at the pinned leaf (ADR 0112 §5).
///
/// # Errors
///
/// [`TurnError::Exec`] when the pin does not decode or the store no longer
/// retains the pinned head; the store's refusal.
pub async fn load_window(
    backend: &Backend,
    session: &SessionId,
    pin: &TurnWindowPin,
) -> Result<TurnWindow, TurnError> {
    let head: SessionHeadRef = serde_json::from_str(pin.as_str()).map_err(|error| {
        TurnError::Exec(format!(
            "window pin `{}` does not decode: {error}",
            pin.as_str()
        ))
    })?;
    if head.revision == 0 {
        return Ok(TurnWindow::new(
            pin.clone(),
            lash_sansio::AppendVec::new(),
            lash_sansio::AppendVec::new(),
        ));
    }
    let store = session_store(backend, session).await?;
    let state = load_state(&store, session, WindowSelector::Admitted(head)).await?;
    Ok(window_of(&state, pin.clone()))
}

/// `state`'s window under `pin`.
fn window_of(state: &RuntimeSessionState, pin: TurnWindowPin) -> TurnWindow {
    let read = state.read_model();
    TurnWindow::new(pin, read.messages, read.active_events)
        .with_render_cache(read.prompt_render_cache)
}

/// The session's window `selector` names as runtime state.
pub(crate) async fn load_state(
    store: &SessionStore,
    session: &SessionId,
    selector: WindowSelector,
) -> Result<RuntimeSessionState, TurnError> {
    let loaded = crate::store::load_session_window_state(store, selector)
        .await
        .map_err(|error| TurnError::Exec(error.to_string()))?
        .ok_or_else(|| TurnError::Exec(format!("session {session} has no head")))?;
    if loaded.state.session_id != *session {
        return Err(TurnError::Exec(format!(
            "session {session}'s store holds session {}",
            loaded.state.session_id
        )));
    }
    Ok(loaded.state)
}
