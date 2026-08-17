// The approval routes: listing pending tool approvals, and submitting operator
// approve / deny decisions that resolve parked durable tool completions.

async fn list_approvals(
    State(state): State<AppState>,
) -> Result<Json<Vec<approvals::PendingApproval>>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    Ok(Json(
        state.approvals.pending().map_err(AppError::internal)?,
    ))
}

async fn approve_wait(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    decide_approval(&state, &key, true).await
}

async fn deny_wait(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    decide_approval(&state, &key, false).await
}

async fn decide_approval(
    state: &AppState,
    key_id: &str,
    approved: bool,
) -> Result<Json<Value>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    let pending = state
        .approvals
        .pending()
        .map_err(AppError::internal)?
        .into_iter()
        .find(|approval| approval.key == key_id)
        .ok_or_else(|| AppError::bad_request(format!("approval `{key_id}` is not pending")))?;
    let key = state
        .approvals
        .completion_key(key_id)
        .map_err(AppError::internal)?;
    let resolution = if approved {
        approvals::approval_resolution(&pending)
    } else {
        approvals::denial_resolution()
    };
    let outcome = state
        .core
        .completions()
        .resolve(key, resolution.clone())
        .await
        .map_err(AppError::internal)?;
    match &outcome {
        lash::ResolveOutcome::Accepted => {}
        lash::ResolveOutcome::AlreadyResolved { terminal } if terminal == &resolution => {}
        lash::ResolveOutcome::AlreadyResolved { .. } => {
            return Err(AppError::bad_request(format!(
                "approval `{key_id}` already has the opposite decision"
            )));
        }
        lash::ResolveOutcome::UnknownOrRevoked => {
            return Err(AppError::bad_request(format!(
                "approval `{key_id}` no longer names an active durable wait"
            )));
        }
    }
    let decision = if approved { "approved" } else { "denied" };
    state
        .approvals
        .mark_decided(key_id, decision)
        .map_err(AppError::internal)?;
    state.trace_for_session(
        &pending.requesting_session,
        "approval.decided",
        json!({
            "key": key_id,
            "tool": pending.tool,
            "arguments": pending.arguments,
            "decision": decision,
            "resolve_outcome": outcome,
        }),
    );
    Ok(Json(json!({
        "key": key_id,
        "decision": decision,
        "outcome": outcome,
    })))
}
