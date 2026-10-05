use super::*;
use lash::SessionId;

// The session-management routes: the roster the session sidebar renders, the create
// flow, and the durable selection a query-less `/api/` call resolves through.
// They live beside the chat routes rather than in them because they are about
// *which* session is served, not about serving one.

/// The workbench's sessions.
///
/// The roster is the durable list. A session the roster has not seen — the boot
/// id of a data directory that predates the roster, or an ad-hoc `?session_id=`
/// tab — is still listed while it is the current one, so the selector never
/// renders a workbench that is serving a session it does not show.
pub(crate) async fn list_sessions(
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
            state.sessions.unrostered_entry(current_session_id.clone()),
        );
    }
    let mut sessions = Vec::with_capacity(rostered.len());
    for entry in rostered {
        sessions.push(SessionView {
            current: entry.session_id == current_session_id,
            session_id: entry.session_id,
            name: entry.name,
            created_at_ms: entry.created_at_ms,
            last_active_ms: entry.last_active_ms,
        });
    }
    Ok(Json(SessionListResponse {
        sessions,
        current_session_id: current_session_id.clone(),
    }))
}

/// The roster row is written before the session is opened, because the row is
/// what the selector lists.
///
/// TypeScript is the sole RLM language (ADR 0096), so the create form offers no
/// language choice and the request carries none.
pub(crate) async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<SessionCreateRequest>,
) -> Result<Json<SessionView>, AppError> {
    let session_id = new_session_id();
    let name = match request.name.as_deref().map(str::trim) {
        None | Some("") => session_id.to_string(),
        Some(name) if name.chars().count() > MAX_SESSION_NAME_CHARS => {
            return Err(AppError::bad_request(format!(
                "session name must be at most {MAX_SESSION_NAME_CHARS} characters"
            )));
        }
        Some(name) => name.to_string(),
    };
    // Creating a session is creating something to observe, so it clears the
    // same gate the routes that read one do.
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let entry = state.sessions.record(session_id.clone(), name);
    // Create the session for the selector and the first `/api/state` poll,
    // then open it once, through the same builder every route uses.
    drop(
        state
            .create_or_open_session(&session_id, "api.sessions")
            .await
            .map_err(|error| state.session_admission_error(&session_id, "api.sessions", error))?,
    );
    state.trace_for_session(
        &session_id,
        "api.sessions.created",
        json!({
            "session_id": session_id,
            "name": entry.name,
        }),
    );
    Ok(Json(SessionView {
        current: session_id == state.current_session_id(),
        session_id: session_id.clone(),
        name: entry.name,
        created_at_ms: entry.created_at_ms,
        last_active_ms: entry.last_active_ms,
    }))
}

/// Only a session on the roster can be selected: selection is what a reload,
/// a restart, and `<data-dir>/session-id` all agree on, and pointing that at a
/// session nothing knows about would leave the selector unable to name it.
pub(crate) async fn select_session(
    State(state): State<AppState>,
    Json(request): Json<SessionSelectRequest>,
) -> Result<Json<SessionView>, AppError> {
    let session_id = state
        .admit_session(
            &SessionQuery {
                session_id: Some(request.session_id.clone()),
            },
            "api.sessions.select",
        )
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let entry = state.sessions.select(&session_id).ok_or_else(|| {
        AppError::not_found(format!("session `{session_id}` is not on the roster"))
    })?;
    state.trace_for_session(
        &session_id,
        "api.sessions.selected",
        json!({ "session_id": session_id }),
    );
    Ok(Json(SessionView {
        current: true,
        session_id: session_id.clone(),
        name: entry.name,
        created_at_ms: entry.created_at_ms,
        last_active_ms: entry.last_active_ms,
    }))
}

/// The composer's sole slash command submits the core's durable compaction
/// command. Restate applies it at a boundary; the host never drives a turn.
pub(crate) async fn compact_context(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Value>, AppError> {
    let session_id = state.admit_session(&query, "api.compact").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::CompactContext {
            session_id: session_id.clone(),
        })?;
    let session = state
        .open_session(&session_id, "api.compact")
        .await
        .map_err(|error| state.session_admission_error(&session_id, "api.compact", error))?;
    match session.admin().state().compact_context(None).await {
        Ok(opened) => Ok(Json(json!({"opened": opened}))),
        Err(lash::EmbedError::Session(lash::SessionError::SessionCommandPending(receipt))) => {
            Ok(Json(json!({"pending": receipt})))
        }
        Err(error) => Err(state.session_admission_error(&session_id, "api.compact", error)),
    }
}

/// Delete one chat from the sidebar.
///
/// The same durable retirement a reset runs, recorded first as a delete, so
/// whichever caller settles it removes the row instead of rotating the slot
/// onto a fresh session. The answer names the chat that takes its place: the
/// most recent one left, or a fresh one when none is.
pub(crate) async fn delete_session(
    AxumPath(session_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<SessionDeleted>, AppError> {
    // A path segment is untrusted: a malformed id is a bad request, never a
    // roster lookup.
    let session_id =
        SessionId::parse(session_id).map_err(|err| AppError::bad_request(err.to_string()))?;
    if state.sessions.entry(&session_id).is_none()
        && state.active_turns.retirement(&session_id).is_none()
    {
        return Err(AppError::not_found(format!(
            "session `{session_id}` is not on the roster"
        )));
    }
    state.sessions.mark_for_removal(&session_id);
    let (successor, replaced_current) = match retire_for_reset(&state, &session_id).await {
        Ok(retired) => retired,
        Err(error) => {
            if state.active_turns.retirement(&session_id) != Some(SessionRetirement::Retired) {
                state.sessions.unmark_for_removal(&session_id);
            }
            return Err(error);
        }
    };
    settle_retired_slot(
        &state,
        &session_id,
        &successor,
        replaced_current,
        "api.sessions.delete",
    )
    .await?;
    Ok(Json(SessionDeleted {
        session_id,
        successor_session_id: successor,
    }))
}
