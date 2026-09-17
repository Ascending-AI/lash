use super::*;
use lash::SessionId;

// Operator durable-wait discovery plus the separate, host-owned approval
// routes. A wait key is not itself an approval request: approval context stays
// in the workbench ledger below.

pub(crate) async fn list_session_waits(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<Vec<lash::AwaitEventKey>>, AppError> {
    let session_id = SessionId::from(session_id);
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    // Returned keys carry resolution authority. This example reuses its
    // deployment-wide operator capability; production hosts should map the
    // same requirement to their own session-aware authorization policy.
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    Ok(Json(
        state
            .core
            .completions()
            .outstanding(&session_id)
            .await
            .map_err(AppError::internal)?,
    ))
}

pub(crate) async fn list_approvals(
    State(state): State<AppState>,
) -> Result<Json<Vec<approvals::PendingApproval>>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    Ok(Json(state.approvals.pending().map_err(AppError::internal)?))
}

pub(crate) async fn approve_wait(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    decide_approval(&state, &key, true).await
}

pub(crate) async fn deny_wait(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    decide_approval(&state, &key, false).await
}

pub(crate) async fn decide_approval(
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
        .find(|approval| approval.key == key_id);
    // The ledger row is the decision's durable record, so it is written first
    // and the wait resolution is derived from it. A crash between the two
    // writes leaves a decided row over an outstanding wait; the decided arm
    // re-drives `resolve` with the recorded decision, which `AlreadyResolved`
    // makes idempotent. The boot reconcile runs the same repair for rows the
    // operator never retries.
    let (decision, key, tool, arguments, requesting_session) = match pending {
        Some(pending) => {
            let not_pending = |error: approvals::ApprovalError| match error {
                approvals::ApprovalError::NotPending(_) => {
                    AppError::bad_request(format!("approval `{key_id}` is not pending"))
                }
                other => AppError::internal(other),
            };
            let key = state
                .approvals
                .completion_key(key_id)
                .map_err(not_pending)?;
            let decision = if approved {
                approvals::ApprovalDecision::Approved
            } else {
                approvals::ApprovalDecision::Denied
            };
            state
                .approvals
                .mark_decided(key_id, decision)
                .map_err(not_pending)?;
            (
                decision,
                key,
                pending.tool,
                pending.arguments,
                pending.requesting_session,
            )
        }
        None => {
            let decided = state
                .approvals
                .decided()
                .map_err(AppError::internal)?
                .into_iter()
                .find(|approval| approval.key == key_id)
                .ok_or_else(|| {
                    AppError::bad_request(format!("approval `{key_id}` is not pending"))
                })?;
            (
                decided.decision,
                decided.completion_key,
                decided.tool,
                decided.arguments,
                decided.requesting_session,
            )
        }
    };
    let resolution = approvals::resolution_for(decision, &arguments);
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
    state.trace_for_session(
        &SessionId::from(requesting_session),
        "approval.decided",
        json!({
            "key": key_id,
            "tool": tool,
            "arguments": arguments,
            "decision": decision.as_str(),
            "resolve_outcome": outcome,
        }),
    );
    Ok(Json(json!({
        "key": key_id,
        "decision": decision.as_str(),
        "outcome": outcome,
    })))
}

/// Boot-time half of the repair: a crash can leave a decided ledger row over a
/// wait that was never resolved, and the pending list no longer shows it for
/// an operator to retry. Re-resolve every decided row; `resolve` is
/// idempotent, so rows whose wait already settled are observed, not re-driven.
pub(crate) async fn reconcile_decided_approvals(state: &AppState) {
    let decided = match state.approvals.decided() {
        Ok(decided) => decided,
        Err(error) => {
            eprintln!("agent-workbench approval reconcile cannot read the ledger: {error}");
            return;
        }
    };
    for decided in decided {
        let resolution = approvals::resolution_for(decided.decision, &decided.arguments);
        match state
            .core
            .completions()
            .resolve(decided.completion_key.clone(), resolution.clone())
            .await
        {
            Ok(lash::ResolveOutcome::Accepted) | Ok(lash::ResolveOutcome::UnknownOrRevoked) => {}
            Ok(lash::ResolveOutcome::AlreadyResolved { terminal }) if terminal == resolution => {}
            Ok(lash::ResolveOutcome::AlreadyResolved { .. }) => eprintln!(
                "agent-workbench approval reconcile: {} was decided {} but the wait resolved differently",
                decided.key,
                decided.decision.as_str()
            ),
            Err(error) => eprintln!(
                "agent-workbench approval reconcile could not resolve {}: {error}",
                decided.key
            ),
        }
    }
}
