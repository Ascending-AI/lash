struct StateProjectionReads {
    read_view: lash::persistence::SessionReadView,
    cursor: SessionCursor,
    pending_turn_inputs: Vec<lash::PendingTurnInput>,
    queued_work: Vec<lash::persistence::QueuedWorkBatch>,
    turn_input_applications: Vec<lash::remote::observations::RemoteTurnInputApplication>,
    usage: lash::usage::SessionUsageReport,
}

fn state_store_request(state: &AppState, session_id: &str) -> lash::persistence::SessionStoreCreateRequest {
    let mut policy = lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded);
    policy.session_id = Some(session_id.to_string());
    policy.model = model_spec_from_selection(state.selected_model());
    lash::persistence::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: lash::persistence::SessionRelation::Root,
        policy,
    }
}

async fn ensure_session_marker_readable(
    state: &AppState,
    session_id: &str,
    surface: &'static str,
) -> Result<(), AppError> {
    let store = state
        .session_store_factory
        .create_store(&state_store_request(state, session_id))
        .await
        .map_err(|error| {
            state.session_admission_error(session_id, surface, lash::EmbedError::Store(error))
        })?;
    store.read_session_state_version().await.map_err(|error| {
        state.session_admission_error(session_id, surface, lash::EmbedError::Store(error))
    })?;
    Ok(())
}

async fn read_state_projection(
    state: &AppState,
    session_id: &str,
    active_turn: bool,
) -> Result<StateProjectionReads, AppError> {
    if !active_turn {
        let session = state.open_session(session_id).await.map_err(|error| {
            state.session_admission_error(session_id, "api.state", error)
        })?;
        let snapshot = session.observe().recoverable_chat_snapshot();
        let pending_turn_inputs = session
            .pending_turn_inputs()
            .await
            .map_err(AppError::internal)?;
        let queued_work = session.queued_work().await.map_err(AppError::internal)?;
        let turn_input_applications = session
            .remote_turn_input_applications()
            .await
            .map_err(AppError::internal)?;
        let usage = session.usage_report();
        return Ok(StateProjectionReads {
            read_view: snapshot.read_view,
            cursor: snapshot.cursor,
            pending_turn_inputs,
            queued_work,
            turn_input_applications,
            usage,
        });
    }

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
            persisted.session_id = session_id.to_string();
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
        read_view: lash::persistence::SessionReadView::from_persisted_state(&persisted),
        cursor,
        pending_turn_inputs,
        queued_work,
        turn_input_applications,
        usage,
    })
}
