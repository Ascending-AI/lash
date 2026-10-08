use super::*;
use lash::SessionId;

// The host-owned approval routes. A pending approval is a parked call of the
// approval tool; its context (arguments, requesting session) stays in the
// workbench ledger.

/// Every pending approval: each live session's parked calls of the approval
/// tool (`Completions::parked`), with the request its call recorded. A call
/// whose wait settled or was revoked (its turn ended) is no longer parked,
/// so it is no longer listed.
pub(crate) async fn pending_approvals(
    state: &AppState,
) -> Result<Vec<approvals::PendingApproval>, AppError> {
    let mut pending = Vec::new();
    for view in state
        .core
        .sessions_filtered(lash::SessionListFilter {
            deleted: Some(false),
            ..Default::default()
        })
        .await
        .map_err(AppError::internal)?
    {
        // Returned keys carry resolution authority: the caller authorized
        // the deployment-wide operator capability.
        for call in state
            .core
            .completions()
            .parked(lash::admin::CallOwner::Session(view.session_id.clone()))
            .await
            .map_err(AppError::internal)?
        {
            if call.tool_id.as_str() != approvals::APPROVAL_TOOL_ID {
                continue;
            }
            if let Some(request) = state
                .approvals
                .undecided(call.call_id.as_str())
                .map_err(AppError::internal)?
            {
                pending.push(request);
            }
        }
    }
    pending.sort_by(|left, right| {
        (left.requested_at_ms, &left.key).cmp(&(right.requested_at_ms, &right.key))
    });
    Ok(pending)
}

pub(crate) async fn list_approvals(
    State(state): State<AppState>,
) -> Result<Json<Vec<approvals::PendingApproval>>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    Ok(Json(pending_approvals(&state).await?))
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
        .undecided(key_id)
        .map_err(AppError::internal)?;
    // The ledger row is the decision's durable record, so it is written first
    // and the wait resolution is derived from it. A crash between the two
    // writes leaves a decided row over an outstanding wait; the decided arm
    // redrives `resolve` with the recorded decision, which `AlreadyResolved`
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
        .resolve(key.as_str(), resolution.clone())
        .await
        .map_err(AppError::internal)?;
    match outcome {
        lash::durable::ResolveAnswer::Resolved | lash::durable::ResolveAnswer::AlreadyResolved => {}
        lash::durable::ResolveAnswer::Conflict => {
            return Err(AppError::bad_request(format!(
                "approval `{key_id}` already has the opposite decision"
            )));
        }
        lash::durable::ResolveAnswer::Unknown
        | lash::durable::ResolveAnswer::Revoked
        | lash::durable::ResolveAnswer::ReservedKind => {
            return Err(AppError::bad_request(format!(
                "approval `{key_id}` no longer names an active durable wait"
            )));
        }
    }
    let outcome = format!("{outcome:?}");
    state.trace_for_session(
        &SessionId::parse(requesting_session)?,
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
/// idempotent, so rows whose wait already settled are observed, not redriven.
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
            .resolve(decided.completion_key.as_str(), resolution)
            .await
        {
            Ok(
                lash::durable::ResolveAnswer::Resolved
                | lash::durable::ResolveAnswer::AlreadyResolved
                | lash::durable::ResolveAnswer::Unknown
                | lash::durable::ResolveAnswer::Revoked
                | lash::durable::ResolveAnswer::ReservedKind,
            ) => {}
            Ok(lash::durable::ResolveAnswer::Conflict) => eprintln!(
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
