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
