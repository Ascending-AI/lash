use super::*;
use lash::SessionId;

pub(crate) struct StateProjectionReads {
    pub(crate) read_view: lash::persistence::SessionReadView,
    pub(crate) cursor: SessionCursor,
    pub(crate) pending_turn_inputs: Vec<lash::PendingTurnInputRead>,
    pub(crate) queued_work: Vec<lash::persistence::QueuedWorkBatch>,
    pub(crate) turn_input_applications: Vec<lash::remote::observations::RemoteTurnInputApplication>,
    pub(crate) usage: lash::usage::SessionUsageReport,
}

impl AppState {
    /// Attaches an observation handle without taking the session's execution
    /// lease.
    ///
    /// `/api/observations` is the page's live turn stream: it writes nothing,
    /// and the page reconnects it every 900 ms for as long as it is refused.
    /// [`lash::SessionBuilder::open`] claims the execution lease to admit the
    /// state it loads, which is the one thing a running turn holds for its
    /// whole length — so every reconnect during a turn burnt the bounded retry
    /// budget and answered 503, and the reconnect loop made that a storm: 156
    /// `session.open.contended` and 27 `session.open.retry_exhausted` records
    /// in one ~25 s turn on the judged `rlm-cell-boundary` run.
    ///
    /// The durable head this attach needs is the same one `/api/state` reads
    /// without a lease, so it is read the same way and handed to
    /// [`lash::SessionBuilder::open_with_state`], which admits nothing and
    /// claims nothing. The builder is still
    /// [`Self::observer_session_builder`]: no model statement, so observing a
    /// session never writes config authority over its settled head
    /// (FIG-3144, FIG-3151).
    pub(crate) async fn open_session_for_observation(
        &self,
        session_id: &SessionId,
    ) -> Result<lash::LashSession, lash::EmbedError> {
        let request = state_store_request(self, session_id);
        let store = self
            .session_store_factory
            .create_store(&request)
            .await
            .map_err(lash::EmbedError::Store)?;
        let state = lash::persistence::load_persisted_session_state(store.as_ref())
            .await
            .map_err(lash::EmbedError::Store)?
            .unwrap_or_else(|| {
                // A session with no durable head yet: the same empty state the
                // `/api/state` projection falls back to, so an observer that
                // attaches before the first commit sees what the snapshot does.
                let mut state = lash::persistence::RuntimeSessionState::new(request.policy.clone());
                state.session_id = session_id.clone();
                state
            });
        self.observer_session_builder(session_id.to_string())
            .store(store)
            .open_with_state(state)
            .await
    }
}

pub(crate) fn state_store_request(
    state: &AppState,
    session_id: &SessionId,
) -> lash::persistence::SessionStoreCreateRequest {
    let mut policy = lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded);
    policy.session_id = Some(SessionId::from(session_id.to_string()));
    policy.model = model_spec_from_selection(state.selected_model());
    lash::persistence::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash::persistence::SessionRelation::Root,
        policy,
    }
}

/// Reads everything `/api/state` projects, without ever taking the session's
/// execution lease.
///
/// This is a probe: the page polls it about twice a second and writes nothing.
/// Opening the session to read it claimed the execution lease, so the poll
/// raced the running turn for the one thing a turn must hold — 242 contended
/// claims and 37 `retry_exhausted` refusals over nine turns in the judged
/// `workbench-continue-as` run, each exhaustion surfacing to the operator as a
/// 503 red banner. The durable store read below answers the same question from
/// the same records and contends with nothing (FIG-3144).
pub(crate) async fn read_state_projection(
    state: &AppState,
    session_id: &SessionId,
) -> Result<StateProjectionReads, AppError> {
    let request = state_store_request(state, session_id);
    let store = state
        .session_store_factory
        .create_store(&request)
        .await
        .map_err(AppError::internal)?;
    let persisted = lash::persistence::load_persisted_session_state(store.as_ref())
        .await
        .map_err(AppError::internal)?
        .unwrap_or_else(|| {
            let mut persisted = lash::persistence::RuntimeSessionState::new(request.policy);
            persisted.session_id = SessionId::from(session_id.to_string());
            persisted
        });
    let revision = persisted
        .checkpoint_ref
        .as_ref()
        .map_or(persisted.turn_index as u64, |_| persisted.head_revision);
    let cursor = SessionCursor::from_store_token(format!(
        "lashsc2:workbench-durable:{revision}:0:{session_id}"
    ))
    .map_err(AppError::internal)?;
    let pending_turn_inputs = store
        .list_pending_turn_inputs(session_id)
        .await
        .map_err(AppError::internal)?;
    let queued_work = store
        .list_pending_queued_work(session_id)
        .await
        .map_err(AppError::internal)?;
    let turn_input_applications = store
        .list_turn_input_applications(session_id)
        .await
        .map_err(AppError::internal)?
        .iter()
        .map(Into::into)
        .collect();
    let usage = persisted.usage_report();
    Ok(StateProjectionReads {
        read_view: lash::persistence::SessionReadView::from_persisted_state(&persisted)
            .map_err(AppError::internal)?,
        cursor,
        pending_turn_inputs,
        queued_work,
        turn_input_applications,
        usage,
    })
}
