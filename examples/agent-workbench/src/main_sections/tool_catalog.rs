//! Browser curation of the session's recorded tools through host commands.
use super::*;
use lash::SessionId;
use lash::tools::ToolId;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tools", get(list_tools))
        .route("/api/tools/membership", post(set_membership))
}

#[derive(Serialize)]
struct ToolListing {
    session_id: SessionId,
    generation: Option<u64>,
    pending: bool,
    tools: Vec<ToolRow>,
}

#[derive(Serialize)]
struct ToolRow {
    id: ToolId,
    name: String,
    member: bool,
    orphaned: bool,
}

async fn list_tools(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<ToolListing>, AppError> {
    let id = state.admit_session(&query, "api.tools").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: id.clone(),
        })?;
    let session = state
        .open_session_for_observation(&id)
        .await
        .map_err(|error| state.session_admission_error(&id, "api.tools", error))?;
    let tools = session
        .admin()
        .tools()
        .state()
        .await
        .map_err(AppError::internal)?;
    Ok(Json(ToolListing {
        session_id: id,
        generation: tools.recorded().map(|recorded| recorded.generation),
        pending: !tools.pending().is_empty(),
        tools: tools
            .recorded()
            .into_iter()
            .flat_map(|recorded| recorded.iter())
            .map(|(id, entry)| ToolRow {
                id: id.clone(),
                name: entry.manifest.name.clone(),
                member: entry.is_member(),
                orphaned: entry.is_orphaned(),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct MembershipRequest {
    tool_id: ToolId,
    member: bool,
    request_id: String,
}

async fn set_membership(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<MembershipRequest>,
) -> Result<Response, AppError> {
    let id = state.admit_session(&query, "api.tools.membership").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageTools {
            session_id: id.clone(),
        })?;
    if request.request_id.trim().is_empty() || request.request_id.len() > 128 {
        return Err(AppError::bad_request("request_id must contain 1-128 bytes"));
    }
    let session = state
        .open_session_for_observation(&id)
        .await
        .map_err(|error| state.session_admission_error(&id, "api.tools", error))?;
    let mutation = session
        .admin()
        .tools()
        .set_membership(
            request.tool_id,
            request.member,
            format!("workbench-tool-membership:{}", request.request_id),
        )
        .await;
    match mutation {
        Ok(lash::AdminMutation::Applied(generation)) => Ok(Json(json!({
            "status": "applied", "session_id": id, "generation": generation,
        }))
        .into_response()),
        Ok(lash::AdminMutation::Pending(receipt)) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending", "session_id": id, "receipt": receipt,
            })),
        )
            .into_response()),
        Err(lash::EmbedError::Reconfigure(cause)) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": cause.to_string(), "cause": cause,
            })),
        )
            .into_response()),
        Err(error) => Err(state.session_admission_error(&id, "api.tools.membership", error)),
    }
}
