use super::*;
use lash::SessionId;

// The host-owned approval routes. A pending approval is a parked call of the
// approval tool, and `Completions::parked` is where its key comes from; its
// context (arguments, requesting session) stays in the workbench ledger.

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

/// The parked approval call `call_id` of `session_id`, with the key that
/// resolves it. `None` once its wait settled or was revoked.
async fn parked_approval(
    state: &AppState,
    session_id: &SessionId,
    call_id: &str,
) -> Result<Option<lash::admin::ParkedCall>, lash::EmbedError> {
    Ok(state
        .core
        .completions()
        .parked(lash::admin::CallOwner::Session(session_id.clone()))
        .await?
        .into_iter()
        .find(|call| {
            call.tool_id.as_str() == approvals::APPROVAL_TOOL_ID && call.call_id.as_str() == call_id
        }))
}

/// Decide the approval `key_id`, the call id its request was recorded under.
///
/// The key is the one `Completions::parked` lists for that call. The ledger
/// row is the decision's durable record, so the decision is written first and
/// the resolution is derived from it; the row is deleted once `resolve`
/// answers. A crash between the two writes leaves a decided row over a call
/// that is still parked: a repeated request, or the boot reconcile, resolves
/// it with the recorded decision. A call that is no longer parked cannot
/// receive a decision, so its row is deleted and the request refused.
pub(crate) async fn decide_approval(
    state: &AppState,
    key_id: &str,
    approved: bool,
) -> Result<Json<Value>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    let not_pending = || AppError::bad_request(format!("approval `{key_id}` is not pending"));
    let request = state
        .approvals
        .request(key_id)
        .map_err(AppError::internal)?
        .ok_or_else(not_pending)?;
    let requesting_session = SessionId::parse(request.requesting_session)?;
    let Some(call) = parked_approval(state, &requesting_session, key_id)
        .await
        .map_err(AppError::internal)?
    else {
        state.approvals.forget(key_id).map_err(AppError::internal)?;
        return Err(not_pending());
    };
    let wanted = if approved {
        approvals::ApprovalDecision::Approved
    } else {
        approvals::ApprovalDecision::Denied
    };
    let opposite = || {
        AppError::bad_request(format!(
            "approval `{key_id}` already has the opposite decision"
        ))
    };
    let decision = state
        .approvals
        .decide(key_id, wanted)
        .map_err(|error| match error {
            approvals::ApprovalError::NotPending(_) => not_pending(),
            other => AppError::internal(other),
        })?;
    if decision != wanted {
        return Err(opposite());
    }
    let outcome = state
        .core
        .completions()
        .resolve(
            call.key.as_str(),
            approvals::resolution_for(decision, &request.arguments),
        )
        .await
        .map_err(AppError::internal)?;
    // `resolve` answered: the row has nothing left to repair.
    state.approvals.forget(key_id).map_err(AppError::internal)?;
    match outcome {
        lash::durable::ResolveAnswer::Resolved | lash::durable::ResolveAnswer::AlreadyResolved => {}
        lash::durable::ResolveAnswer::Conflict => return Err(opposite()),
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
        &requesting_session,
        "approval.decided",
        json!({
            "key": key_id,
            "tool": request.tool,
            "arguments": request.arguments,
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

/// Boot-time half of the repair, over every ledger row. A decided row whose
/// call is still parked is the crash between the decision and its resolve:
/// resolve it with the key `parked` lists and delete the row once `resolve`
/// answers. A row whose call is no longer parked, decided or not, has nothing
/// left to receive a decision and is deleted.
pub(crate) async fn reconcile_approvals(state: &AppState) {
    let requests = match state.approvals.requests() {
        Ok(requests) => requests,
        Err(error) => {
            eprintln!("agent-workbench approval reconcile cannot read the ledger: {error}");
            return;
        }
    };
    for request in requests {
        if let Err(error) = reconcile_approval(state, &request).await {
            eprintln!(
                "agent-workbench approval reconcile could not settle {}: {error}",
                request.call_id
            );
        }
    }
}

async fn reconcile_approval(
    state: &AppState,
    request: &approvals::ApprovalRequest,
) -> AnyhowResult<()> {
    let session_id = SessionId::parse(request.requesting_session.clone())?;
    let Some(call) = parked_approval(state, &session_id, &request.call_id).await? else {
        state.approvals.forget(&request.call_id)?;
        return Ok(());
    };
    let Some(decision) = request.decision else {
        // Still waiting for its operator.
        return Ok(());
    };
    let answer = state
        .core
        .completions()
        .resolve(
            call.key.as_str(),
            approvals::resolution_for(decision, &request.arguments),
        )
        .await?;
    if answer == lash::durable::ResolveAnswer::Conflict {
        eprintln!(
            "agent-workbench approval reconcile: {} was decided {} but the wait resolved differently",
            request.call_id,
            decision.as_str()
        );
    }
    state.approvals.forget(&request.call_id)?;
    Ok(())
}
