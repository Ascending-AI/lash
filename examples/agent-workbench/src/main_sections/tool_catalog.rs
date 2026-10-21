//! Browser selection of the session's mail tools through the workbench's
//! own config.
//!
//! What a session is offered is its providers' config (R2): the listing
//! reads what the session's providers offer under its recorded config, and
//! a change writes the workbench's mail-tool selection with its config
//! command. A tool outside the selection stays listed so it can be offered
//! again.
use super::*;
use lash::tools::ToolId;
use lash_vocabulary::identity::SessionId;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tools", get(list_tools))
        .route("/api/tools/selection", post(set_selection))
}

#[derive(Serialize)]
struct ToolListing {
    session_id: SessionId,
    tools: Vec<ToolRow>,
}

#[derive(Serialize)]
struct ToolRow {
    id: ToolId,
    name: String,
    /// The session's providers offer it under its recorded config.
    offered: bool,
    /// The workbench's selection decides whether it is offered.
    selectable: bool,
}

/// The tools the session is offered, then the mail tools its selection
/// leaves out.
async fn listing(
    state: &AppState,
    session: &lash::LashSession,
) -> Result<Vec<ToolRow>, lash::EmbedError> {
    let offered = session.admin().offered_tools().await?;
    let mail = mail::MockMailProvider::new(state.mail_world.clone(), state.host_triggers.clone())
        .all_manifests();
    let selectable = mail
        .iter()
        .map(|manifest| manifest.id.clone())
        .collect::<BTreeSet<_>>();
    let offered_ids = offered
        .iter()
        .map(|manifest| manifest.id.clone())
        .collect::<BTreeSet<_>>();
    let mut rows = offered
        .into_iter()
        .map(|manifest| ToolRow {
            selectable: selectable.contains(&manifest.id),
            name: manifest.tool(),
            id: manifest.id,
            offered: true,
        })
        .collect::<Vec<_>>();
    rows.extend(
        mail.into_iter()
            .filter(|manifest| !offered_ids.contains(&manifest.id))
            .map(|manifest| ToolRow {
                name: manifest.tool(),
                id: manifest.id,
                offered: false,
                selectable: true,
            }),
    );
    Ok(rows)
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
    let tools = listing(&state, &session)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(ToolListing {
        session_id: id,
        tools,
    }))
}

#[derive(Deserialize)]
struct SelectionRequest {
    tool_id: ToolId,
    offered: bool,
    request_id: String,
}

async fn set_selection(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<SelectionRequest>,
) -> Result<Response, AppError> {
    let id = state.admit_session(&query, "api.tools.selection").await?;
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
    let rows = listing(&state, &session)
        .await
        .map_err(AppError::internal)?;
    if !rows
        .iter()
        .any(|row| row.selectable && row.id == request.tool_id)
    {
        return Err(AppError::bad_request(
            "only the workbench's mail tools are selectable",
        ));
    }
    // The selection names what stays out, so a mail account added later is
    // offered until the user leaves it out.
    let excluded = rows
        .iter()
        .filter(|row| row.selectable)
        .filter(|row| {
            if row.id == request.tool_id {
                !request.offered
            } else {
                !row.offered
            }
        })
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    let config = session.admin().config();
    let revision = config
        .revision()
        .await
        .map_err(|error| state.session_admission_error(&id, "api.tools.selection", error))?;
    let settlement = config
        .apply(
            lash::config::ConfigWrite::new(
                format!("workbench-tool-selection:{}", request.request_id),
                revision,
            ),
            lash::config::ConfigTransaction::of(SetWorkbenchTools {
                tools: lash::tools::ToolSelection::AllExcept(excluded),
            }),
        )
        .await
        .map_err(|error| state.session_admission_error(&id, "api.tools.selection", error))?;
    Ok(match settlement {
        lash::config::ConfigSettlement::Settled(
            lash::config::ConfigTransactionOutcome::Applied { revision, .. },
        ) => Json(json!({
            "status": "applied", "session_id": id, "revision": revision,
        }))
        .into_response(),
        lash::config::ConfigSettlement::Pending(receipt) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending", "session_id": id, "receipt": receipt,
            })),
        )
            .into_response(),
        other => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("the tool selection did not apply: {other:?}"),
            })),
        )
            .into_response(),
    })
}
