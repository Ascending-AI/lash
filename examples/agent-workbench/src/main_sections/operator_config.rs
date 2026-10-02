use super::*;

pub(super) fn operator_config_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/sessions/{session_id}/config",
            get(operator_config_read).post(operator_config_submit),
        )
        .route(
            "/api/admin/sessions/{session_id}/config/settle",
            post(operator_config_settle),
        )
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", content = "args", deny_unknown_fields)]
pub(crate) enum OperatorConfigCommand {
    #[serde(rename = "set_llm_profile")]
    LlmProfile(lash::config::SetLlmProfile),
    #[serde(rename = "set_reasoning")]
    Reasoning(lash::config::SetReasoning),
    #[serde(rename = "set_turn_budget")]
    TurnBudget(lash::config::SetTurnBudget),
    #[serde(rename = "set_max_tool_calls")]
    MaxToolCalls(lash::config::SetMaxToolCalls),
    #[serde(rename = "set_tool_access")]
    ToolAccess(lash::config::SetToolAccess),
    #[serde(rename = "set_autonomy")]
    Autonomy(lash::config::SetAutonomy),
    #[serde(rename = "set_charge_safety")]
    ChargeSafety(lash::config::SetChargeSafety),
    #[serde(rename = "set_no_progress_budget")]
    NoProgressBudget(lash::config::SetNoProgressBudget),
    #[serde(rename = "set_generation")]
    Generation(lash::config::SetGeneration),
    #[serde(rename = "set_attachment_acceptance")]
    AttachmentAcceptance(lash::config::SetAttachmentAcceptance),
    #[serde(rename = "set_standard_prompt")]
    StandardPrompt(lash::standard::SetStandardPrompt),
    #[serde(rename = "set_standard_prompt_context")]
    StandardPromptContext(lash::standard::SetStandardPromptContext),
    #[serde(rename = "set_standard_render")]
    StandardRender(lash::standard::SetStandardRender),
    #[serde(rename = "set_rlm_prompt")]
    RlmPrompt(lash::rlm::SetRlmPrompt),
    #[serde(rename = "set_rlm_prompt_context")]
    RlmPromptContext(lash::rlm::SetRlmPromptContext),
    #[serde(rename = "set_rlm_render")]
    RlmRender(lash::rlm::SetRlmRender),
}
impl OperatorConfigCommand {
    fn append(
        self,
        transaction: lash::config::ConfigTransaction,
    ) -> lash::config::ConfigTransaction {
        match self {
            Self::LlmProfile(command) => transaction.then(command),
            Self::Reasoning(command) => transaction.then(command),
            Self::TurnBudget(command) => transaction.then(command),
            Self::MaxToolCalls(command) => transaction.then(command),
            Self::ToolAccess(command) => transaction.then(command),
            Self::Autonomy(command) => transaction.then(command),
            Self::ChargeSafety(command) => transaction.then(command),
            Self::NoProgressBudget(command) => transaction.then(command),
            Self::Generation(command) => transaction.then(command),
            Self::AttachmentAcceptance(command) => transaction.then(command),
            Self::StandardPrompt(command) => transaction.then(command),
            Self::StandardPromptContext(command) => transaction.then(command),
            Self::StandardRender(command) => transaction.then(command),
            Self::RlmPrompt(command) => transaction.then(command),
            Self::RlmPromptContext(command) => transaction.then(command),
            Self::RlmRender(command) => transaction.then(command),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorConfigRequest {
    pub(crate) id: String,
    pub(crate) expected_revision: u64,
    pub(crate) commands: Vec<OperatorConfigCommand>,
}

async fn operator_config_read(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let durable = state.core.session(session_id.clone()).durable().await?;
    let snapshot = durable.read().await?;
    // Discovery supplies the registered schemas and their revision; values
    // below are the committed view, never a resident runtime's config.
    let session = state
        .open_session(&session_id, "operator_config_catalog")
        .await?;
    let catalog = session.admin().config().commands().await?;
    Ok(Json(
        json!({"catalog": catalog, "recorded": snapshot.map(|view| {
            let snapshot = view.to_snapshot();
            json!({"policy": snapshot.policy, "plugins": snapshot.plugin_config})
        })}),
    ))
}

async fn operator_config_submit(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(request): Json<OperatorConfigRequest>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    let session = state
        .open_session(&session_id, "operator_config_submit")
        .await?;
    let transaction = request.commands.into_iter().fold(
        lash::config::ConfigTransaction::new(),
        |transaction, command| command.append(transaction),
    );
    let settlement = session
        .admin()
        .config()
        .submit(
            lash::config::ConfigWrite::new(request.id, request.expected_revision),
            transaction,
        )
        .await?;
    Ok(Json(config_settlement(settlement)))
}

async fn operator_config_settle(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    Json(receipt): Json<lash::SessionCommandReceipt>,
) -> Result<Json<Value>, OperatorError> {
    authorize_operator(&state)?;
    check_receipt_session(&session_id, &receipt)?;
    let session = state
        .open_session(&session_id, "operator_config_settle")
        .await?;
    Ok(Json(config_settlement(
        session.admin().config().settle(receipt).await?,
    )))
}
fn config_settlement(settlement: lash::config::ConfigSettlement) -> Value {
    match settlement {
        lash::config::ConfigSettlement::Settled(outcome) => {
            json!({"kind": "settled", "outcome": outcome})
        }
        lash::config::ConfigSettlement::Pending(receipt) => {
            json!({"kind": "pending", "receipt": receipt})
        }
        lash::config::ConfigSettlement::Cancelled(receipt) => {
            json!({"kind": "cancelled", "receipt": receipt})
        }
    }
}
