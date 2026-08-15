// The session-management routes: the roster the selector renders, the
// create-with-a-dialect flow, and the durable selection a query-less `/api/`
// call resolves through. They live beside the chat routes rather than in them
// because they are about *which* session is served, not about serving one.

/// The workbench's sessions, each labelled with the dialect it recorded.
///
/// The roster is the durable list; the recorded dialect is read back from each
/// session, because the roster row says what was *asked* for and only the
/// session says what it runs. A session the roster has not seen — the boot id
/// of a data directory that predates the roster, or an ad-hoc `?session_id=`
/// tab — is still listed while it is the current one, so the selector never
/// renders a workbench that is serving a session it does not show.
async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<SessionListResponse>, AppError> {
    let current_session_id = state.current_session_id();
    let mut rostered = state.sessions.list();
    if !rostered
        .iter()
        .any(|entry| entry.session_id == current_session_id)
    {
        rostered.insert(
            0,
            state.sessions.unrostered_entry(
                current_session_id.clone(),
                state.requested_dialect(&current_session_id),
            ),
        );
    }
    let mut sessions = Vec::with_capacity(rostered.len());
    for entry in rostered {
        let dialect = state.recorded_dialect(&entry.session_id).await;
        sessions.push(SessionSummary {
            current: entry.session_id == current_session_id,
            dialect: dialect.language_id(),
            session_id: entry.session_id,
            name: entry.name,
            created_at_ms: entry.created_at_ms,
            last_active_ms: entry.last_active_ms,
        });
    }
    Ok(Json(SessionListResponse {
        sessions,
        current_session_id,
        dialects: lash::rlm::RlmDialect::ALL
            .iter()
            .map(|dialect| dialect.language_id())
            .collect(),
        default_dialect: state.rlm_dialect.language_id(),
    }))
}

/// Add a session, pinned to the dialect the operator picked.
///
/// The roster row is written before the session is opened, because the row is
/// where every later open reads the dialect to ask for: the pin only becomes
/// durable at the first commit, and the handle this route opens does not commit.
async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<SessionCreateRequest>,
) -> Result<Json<SessionSummary>, AppError> {
    let dialect = match request.dialect.as_deref().map(str::trim) {
        None | Some("") => state.rlm_dialect,
        Some(language_id) => lash::rlm::RlmDialect::from_language_id(language_id).ok_or_else(
            || {
                AppError::bad_request(format!(
                    "`{language_id}` is not a registered RLM dialect; the registered language ids are {}",
                    lash::rlm::RlmDialect::registered_language_ids()
                ))
            },
        )?,
    };
    let session_id = new_session_id();
    let name = match request.name.as_deref().map(str::trim) {
        None | Some("") => session_id.clone(),
        Some(name) if name.chars().count() > MAX_SESSION_NAME_CHARS => {
            return Err(AppError::bad_request(format!(
                "session name must be at most {MAX_SESSION_NAME_CHARS} characters"
            )));
        }
        Some(name) => name.to_string(),
    };
    let entry = state.sessions.record(session_id.clone(), name, dialect);
    // Open once so the session exists for the selector and the first `/api/state`
    // poll, through the same builder every route uses.
    drop(
        state
            .open_session(&session_id)
            .await
            .map_err(|error| state.session_admission_error(&session_id, "api.sessions", error))?,
    );
    state.trace_for_session(
        &session_id,
        "api.sessions.created",
        json!({
            "session_id": session_id,
            "name": entry.name,
            "dialect": dialect.language_id(),
        }),
    );
    Ok(Json(SessionSummary {
        current: session_id == state.current_session_id(),
        dialect: state.recorded_dialect(&session_id).await.language_id(),
        session_id,
        name: entry.name,
        created_at_ms: entry.created_at_ms,
        last_active_ms: entry.last_active_ms,
    }))
}

/// Make a rostered session the one a query-less call resolves to.
///
/// Only a session on the roster can be selected: selection is what a reload,
/// a restart, and `<data-dir>/session-id` all agree on, and pointing that at a
/// session nothing knows about would leave the selector unable to name it.
async fn select_session(
    State(state): State<AppState>,
    Json(request): Json<SessionSelectRequest>,
) -> Result<Json<SessionSummary>, AppError> {
    let session_id = SessionQuery {
        session_id: Some(request.session_id.clone()),
    }
    .resolve(&state)?;
    let entry = state.sessions.select(&session_id).ok_or_else(|| {
        AppError::not_found(format!("session `{session_id}` is not on the roster"))
    })?;
    state.trace_for_session(
        &session_id,
        "api.sessions.selected",
        json!({ "session_id": session_id }),
    );
    Ok(Json(SessionSummary {
        current: true,
        dialect: state.recorded_dialect(&session_id).await.language_id(),
        session_id,
        name: entry.name,
        created_at_ms: entry.created_at_ms,
        last_active_ms: entry.last_active_ms,
    }))
}
