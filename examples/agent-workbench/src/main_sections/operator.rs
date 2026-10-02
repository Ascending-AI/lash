use super::*;
use lash::{BuildGeneration, ObligationId, ObligationKind, ParkedWorkRef, SessionId};
use std::num::{NonZeroU32, NonZeroUsize};

pub(crate) fn operator_routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/parks", get(operator_parks))
        .route("/api/admin/parks/events", get(operator_park_events))
        .route("/api/admin/parks/redrive", post(operator_redrive))
        .route("/api/admin/parks/cancel", post(operator_cancel_park))
        .route("/api/admin/parks/fork", post(operator_fork_park))
        .route("/api/admin/drain", get(operator_drain))
        .route(
            "/api/admin/generations/{generation}/drain",
            get(operator_generation_status)
                .post(operator_generation_drain)
                .delete(operator_generation_end),
        )
        .route("/api/admin/obligations/{kind}", get(operator_stalls))
        .route("/api/admin/obligations/rearm", post(operator_rearm))
        .route(
            "/api/admin/sessions/{session_id}/commands",
            post(operator_command_submit),
        )
        .route(
            "/api/admin/sessions/{session_id}/commands/withdraw",
            post(operator_command_withdraw),
        )
        .route(
            "/api/admin/sessions/{session_id}/commands/settle",
            post(operator_command_settle),
        )
        .route(
            "/api/admin/sessions/{session_id}/compact",
            post(operator_compact),
        )
        .merge(operator_config_routes())
}

// This example's operator policy is separate from chat participation. A host
// replacing allow_all() can deny every operator route with this one action.
fn authorize_operator(state: &AppState) -> Result<(), OperatorError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::OperateDeployment)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperatorError {
    #[error(transparent)]
    Authorization(#[from] AppError),
    #[error(transparent)]
    Core(Box<lash::EmbedError>),
    #[error(transparent)]
    Park(Box<lash::ParkVerbRefused>),
}

impl From<lash::EmbedError> for OperatorError {
    fn from(error: lash::EmbedError) -> Self {
        Self::Core(Box::new(error))
    }
}
impl From<lash::ParkVerbRefused> for OperatorError {
    fn from(error: lash::ParkVerbRefused) -> Self {
        Self::Park(Box::new(error))
    }
}

impl IntoResponse for OperatorError {
    fn into_response(self) -> Response {
        use lash::{EmbedError, ParkVerbRefused};
        let (status, cause) = match self {
            Self::Authorization(error) => return error.into_response(),
            Self::Core(error) => {
                let status = if error.is_retryable() {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::CONFLICT
                };
                let cause = match *error {
                    EmbedError::UnknownSession { session_id } => {
                        json!({"kind": "unknown_session", "session_id": session_id})
                    }
                    EmbedError::SessionCreationUnrecorded { session_id } => {
                        json!({"kind": "creation_unrecorded", "session_id": session_id})
                    }
                    EmbedError::Plugin(error) => json!({"kind": "plugin", "error": error}),
                    EmbedError::Session(lash::SessionError::Store { source, .. }) => {
                        json!({"kind": "store", "error": lash::runtime::RuntimeEffectControllerError::from(source)})
                    }
                    EmbedError::Session(lash::SessionError::Plugin(error)) => {
                        json!({"kind": "plugin", "error": error})
                    }
                    EmbedError::Session(lash::SessionError::SessionConfigRefused(refusal)) => {
                        json!({"kind": "config_refused", "refusal": refusal})
                    }
                    EmbedError::Runtime(error) => json!({"kind": "runtime", "error": error}),
                    EmbedError::Store(error) => {
                        json!({"kind": "store", "error": lash::runtime::RuntimeEffectControllerError::from(error)})
                    }
                    EmbedError::ConfigSubmit(error) => config_submit_cause(error),
                    EmbedError::DrainOwnGeneration { generation } => {
                        json!({"kind": "drain_own_generation", "generation": generation})
                    }
                    EmbedError::Session(lash::SessionError::SessionCommandPending(receipt)) => {
                        json!({"kind": "command_pending", "receipt": receipt})
                    }
                    EmbedError::Session(lash::SessionError::SessionCommandCancelled(receipt)) => {
                        json!({"kind": "command_cancelled", "receipt": receipt})
                    }
                    other => json!({"kind": "session", "message": other.to_string()}),
                };
                (status, cause)
            }
            Self::Park(error) => {
                let cause = match *error {
                    ParkVerbRefused::NotParked => json!({"kind": "not_parked"}),
                    ParkVerbRefused::ParkSuperseded { current } => {
                        json!({"kind": "park_superseded", "current": current})
                    }
                    ParkVerbRefused::Redriving { intent } => {
                        json!({"kind": "redriving", "intent": intent})
                    }
                    ParkVerbRefused::IntentOpen { intent } => {
                        json!({"kind": "intent_open", "intent": intent})
                    }
                    ParkVerbRefused::SessionDeleted => json!({"kind": "session_deleted"}),
                    ParkVerbRefused::SessionClosing => json!({"kind": "session_closing"}),
                    ParkVerbRefused::ForkRequiresTurn => json!({"kind": "fork_requires_turn"}),
                    ParkVerbRefused::Store(error) => {
                        json!({"kind": "store", "error": lash::runtime::RuntimeEffectControllerError::from(error)})
                    }
                    ParkVerbRefused::SubstrateRefused { code, message } => {
                        json!({"kind": "engine_refused", "code": code, "message": message})
                    }
                    other => json!({"kind": "park_refused", "message": other.to_string()}),
                };
                (StatusCode::CONFLICT, cause)
            }
        };
        (status, Json(json!({"cause": cause}))).into_response()
    }
}

fn config_submit_cause(error: lash::config::ConfigSubmitError) -> Value {
    use lash::config::ConfigSubmitError;
    match error {
        ConfigSubmitError::Empty => json!({"kind": "empty_transaction"}),
        ConfigSubmitError::UnknownOwner { owner } => {
            json!({"kind": "unknown_owner", "owner": owner})
        }
        ConfigSubmitError::UnknownCommand { owner, command } => {
            json!({"kind": "unknown_command", "owner": owner, "command": command})
        }
        ConfigSubmitError::InvalidArgs {
            owner,
            command,
            detail,
        } => {
            json!({"kind": "invalid_arguments", "owner": owner, "command": command, "detail": detail})
        }
        ConfigSubmitError::ChangedContent { id } => json!({"kind": "changed_content", "id": id}),
        ConfigSubmitError::Registration(error) => {
            json!({"kind": "registration", "message": error.to_string()})
        }
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct OperatorPageQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: Option<NonZeroU32>,
}
impl OperatorPageQuery {
    fn limit(&self) -> Result<NonZeroU32, AppError> {
        let limit = self.limit.unwrap_or(NonZeroU32::MIN.saturating_add(49));
        if limit.get() > 200 {
            return Err(AppError::bad_request("page limit must be at most 200"));
        }
        Ok(limit)
    }
    fn cursor<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>, AppError> {
        self.after
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| AppError::bad_request(format!("invalid page cursor: {error}")))
    }
}

pub(crate) async fn operator_parks(
    State(state): State<AppState>,
    Query(page): Query<OperatorPageQuery>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let mut query = lash::ParkedWorkQuery::all(
        NonZeroUsize::MIN.saturating_add(page.limit()?.get() as usize - 1),
    );
    query.after = page.cursor()?;
    let page = state.core.parked_work().list(&query).await?;
    Ok(Json(
        json!({"records": page.records.into_iter().map(|park| json!({"target": park.target, "park_id": park.park_id, "reason": park.reason, "since_ms": park.since_ms, "last_refused_ms": park.last_refused_ms, "attempts": park.attempts})).collect::<Vec<_>>(), "next": page.next}),
    ))
}

pub(crate) async fn operator_park_events(
    State(state): State<AppState>,
    Query(page): Query<OperatorPageQuery>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let cursor = page
        .cursor()?
        .unwrap_or_else(lash::ParkedWorkEventsCursor::initial);
    let page = state
        .core
        .parked_work()
        .events(
            &cursor,
            NonZeroUsize::MIN.saturating_add(page.limit()?.get() as usize - 1),
        )
        .await?;
    Ok(Json(
        json!({"events": page.events.into_iter().map(|event| json!({"target": event.target, "park_id": event.park_id, "at_ms": event.at_ms, "kind": event.kind})).collect::<Vec<_>>(), "next": page.next}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorParkRequest {
    pub(crate) target: ParkedWorkRef,
    pub(crate) park_id: lash::persistence::ParkId,
}

pub(crate) async fn operator_redrive(
    State(state): State<AppState>,
    Json(request): Json<OperatorParkRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let result = state
        .core
        .parked_work()
        .redrive(&request.target, request.park_id)
        .await?;
    Ok(Json(match result {
        lash::RedriveAccepted::Run(result) => {
            json!({"kind": "turn", "intent": result.intent, "applied": result.applied, "turn_id": result.run})
        }
        lash::RedriveAccepted::Process { process, park } => {
            json!({"kind": "process", "process_id": process, "park_id": park})
        }
    }))
}
pub(crate) async fn operator_cancel_park(
    State(state): State<AppState>,
    Json(request): Json<OperatorParkRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let result = state
        .core
        .parked_work()
        .cancel(&request.target, request.park_id)
        .await?;
    Ok(Json(
        json!({"intent": result.intent, "terminal": result.terminal, "applied": result.applied}),
    ))
}
pub(crate) async fn operator_fork_park(
    State(state): State<AppState>,
    Json(request): Json<OperatorParkRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let ParkedWorkRef::Turn {
        session_id,
        turn_id,
    } = request.target
    else {
        return Err(lash::ParkVerbRefused::ForkRequiresTurn.into());
    };
    let result = state
        .core
        .parked_work()
        .fork(&session_id, &turn_id, request.park_id)
        .await?;
    Ok(Json(
        json!({"intent": result.intent, "cancelled": result.cancelled, "new_turn": result.new_run, "applied": result.applied}),
    ))
}

#[derive(Deserialize)]
pub(crate) struct OperatorDrainQuery {
    pub(crate) accepting_new_work: bool,
}
pub(crate) async fn operator_drain(
    State(state): State<AppState>,
    Query(query): Query<OperatorDrainQuery>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    Ok(Json(json!(
        state.core.drain_status(query.accepting_new_work).await?
    )))
}
pub(crate) async fn operator_generation_status(
    State(state): State<AppState>,
    AxumPath(generation): AxumPath<BuildGeneration>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    Ok(Json(json!(
        state.core.generation_drain_status(&generation).await?
    )))
}
pub(crate) async fn operator_generation_drain(
    State(state): State<AppState>,
    AxumPath(generation): AxumPath<BuildGeneration>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    Ok(Json(
        json!({"changed": state.core.drain_generation(&generation).await?}),
    ))
}
pub(crate) async fn operator_generation_end(
    State(state): State<AppState>,
    AxumPath(generation): AxumPath<BuildGeneration>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    Ok(Json(
        json!({"changed": state.core.end_generation_drain(&generation).await?}),
    ))
}
pub(crate) async fn operator_stalls(
    State(state): State<AppState>,
    AxumPath(kind): AxumPath<ObligationKind>,
    Query(page): Query<OperatorPageQuery>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let limit = page.limit()?.get() as usize;
    let after = page.after.map(ObligationId::new);
    let records = state
        .core
        .stalled_obligations(
            kind,
            after.as_ref(),
            NonZeroUsize::MIN.saturating_add(limit),
        )
        .await?;
    let next = (records.len() > limit).then(|| records[limit - 1].id.clone());
    Ok(Json(
        json!({"records": records.into_iter().take(limit).map(|record| json!({"kind": record.kind, "id": record.id, "reason": record.reason, "attempts": record.attempts, "last_error": record.last_error, "stalled_at_ms": record.stalled_at_ms})).collect::<Vec<_>>(), "next": next}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorRearmRequest {
    pub(crate) kind: ObligationKind,
    pub(crate) id: ObligationId,
}
pub(crate) async fn operator_rearm(
    State(state): State<AppState>,
    Json(request): Json<OperatorRearmRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    Ok(Json(
        json!({"rearmed": state.core.rearm_obligation(request.kind, &request.id).await?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorCommandRequest {
    pub(crate) id: String,
    pub(crate) command: lash::SessionCommand,
}
pub(crate) async fn operator_command_submit(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(request): Json<OperatorCommandRequest>,
) -> Result<Json<lash::SessionCommandReceipt>, OperatorError> {
    authorize_operator(&state)?;
    if matches!(
        request.command,
        lash::SessionCommand::ApplyConfigTransaction { .. }
    ) {
        return Err(AppError::bad_request("use the typed config route for configuration").into());
    }
    let session = state
        .open_session(&session_id, "operator_command_submit")
        .await?;
    Ok(Json(
        session
            .admin()
            .commands()
            .submit(request.command, request.id)
            .await?,
    ))
}
fn check_receipt_session(
    session_id: &SessionId,
    receipt: &lash::SessionCommandReceipt,
) -> Result<(), AppError> {
    if receipt.session_id != session_id {
        return Err(AppError::bad_request(
            "command receipt belongs to another session",
        ));
    }
    Ok(())
}
pub(crate) async fn operator_command_withdraw(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(receipt): Json<lash::SessionCommandReceipt>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    check_receipt_session(&session_id, &receipt)?;
    let session = state
        .open_session(&session_id, "operator_command_withdraw")
        .await?;
    let outcome = session.admin().commands().withdraw(&receipt).await?;
    Ok(Json(
        json!({"kind": match outcome { lash::SessionCommandWithdrawal::Withdrawn => "withdrawn", lash::SessionCommandWithdrawal::AlreadyAdmitted => "already_admitted" }, "receipt": receipt}),
    ))
}
pub(crate) async fn operator_command_settle(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(receipt): Json<lash::SessionCommandReceipt>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    check_receipt_session(&session_id, &receipt)?;
    let session = state
        .open_session(&session_id, "operator_command_settle")
        .await?;
    Ok(Json(command_settlement(
        session.admin().commands().settle(receipt).await?,
    )))
}
fn command_settlement(settlement: lash::SessionCommandSettlement) -> Value {
    match settlement {
        lash::SessionCommandSettlement::Rejected(error) => {
            json!({"kind": "rejected", "error": error})
        }
        lash::SessionCommandSettlement::Durable(receipt) => {
            json!({"kind": "durable", "receipt": receipt})
        }
        lash::SessionCommandSettlement::Pending(receipt) => {
            json!({"kind": "pending", "receipt": receipt})
        }
        lash::SessionCommandSettlement::Cancelled(receipt) => {
            json!({"kind": "cancelled", "receipt": receipt})
        }
        lash::SessionCommandSettlement::Applied { receipt, outcome } => {
            json!({"kind": "applied", "receipt": receipt, "outcome": outcome})
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorCompactRequest {
    pub(crate) instructions: Option<String>,
}
pub(crate) async fn operator_compact(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(request): Json<OperatorCompactRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let session = state.open_session(&session_id, "operator_compact").await?;
    Ok(Json(
        json!({"opened": session.admin().state().compact_context(request.instructions).await?}),
    ))
}

#[path = "operator_config.rs"]
mod operator_config;
use operator_config::*;
