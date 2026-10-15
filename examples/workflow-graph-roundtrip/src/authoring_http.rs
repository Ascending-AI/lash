//! Example-owned HTTP adapter for Lash's public authoring API.
//! The planner knows no HTTP, revisions, example catalog or publication policy.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lash::workflow::WorkflowImport;
use lash::workflow::authoring::{Operation, plan};
use lash::workflow::document::{Correspondence, Document, Name};
use lash::workflow::edit::{Draft, Transaction};
use serde::{Deserialize, Serialize};

use crate::{AppState, ErrorResponse, Origin, WorkflowView, runtime};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRequest {
    version: u64,
    source: String,
    entry: Name,
}

pub(crate) async fn import_source(
    State(state): State<AppState>,
    Json(request): Json<SourceRequest>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    check_version(request.version, current.version)?;
    let document = import(&state.core, &request.source).await?;
    if !document.entries.contains_key(&request.entry) {
        return Err(ErrorResponse::invalid(
            "entry",
            "Select an exported workflow function",
        ));
    }
    let draft = Draft::open(document).map_err(|error| ErrorResponse::invalid("document", error))?;
    let checked = state.environment().await?;
    let (saved, _) = state
        .install(draft, request.entry, Origin::Imported(request.source))
        .await;
    Ok(Json(saved.view(checked.functions())))
}

/// Lowers authored TypeScript through lash's workflow import and admits it
/// against the run environment, as written: the host rewrites nothing.
pub(crate) async fn import(core: &lash::LashCore, source: &str) -> Result<Document, ErrorResponse> {
    let environment =
        runtime::environment().map_err(|error| ErrorResponse::invalid("environment", error))?;
    let imported = core
        .host_artifacts()
        .import_typescript_workflow(source, &environment)
        .await
        .map_err(|error| ErrorResponse::invalid("source_import", error))?;
    match imported {
        WorkflowImport::Imported { document } => Ok(document.document().clone()),
        WorkflowImport::Refused(diagnostics) => Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "source_refused",
            "TypeScript import was refused; the current document is unchanged",
            serde_json::json!({"diagnostics": diagnostics.iter().map(|diagnostic| {
                serde_json::json!({
                    "message": diagnostic.message,
                    "span": diagnostic.span.map(|span| serde_json::json!({"start":span.start,"end":span.end})),
                })
            }).collect::<Vec<_>>()}),
        )),
        WorkflowImport::Admission(refusal) => {
            Err(ErrorResponse::invalid("source_admission", refusal))
        }
        other => Err(ErrorResponse::invalid(
            "source_unsupported",
            format!("{other:?}"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRequest {
    version: u64,
    operations: Vec<Operation>,
}

#[derive(Serialize)]
pub(crate) struct PlanResponse {
    version: u64,
    transaction: Transaction,
    preview: WorkflowView,
    correspondence: Correspondence,
}

pub(crate) async fn preview(
    State(state): State<AppState>,
    Json(request): Json<PlanRequest>,
) -> Result<Json<PlanResponse>, ErrorResponse> {
    let environment = state.environment().await?;
    let mut current = state.current();
    check_version(request.version, current.version)?;
    let transaction = plan(&current.draft, request.operations);
    let applied = current
        .draft
        .apply(&transaction, &environment.checker())
        .map_err(|refusal| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "plan_refused",
                "The batch was refused; the current document is unchanged",
                serde_json::json!({"transaction": &transaction, "diagnostics": refusal.diagnostics.iter().map(|diagnostic| {
                serde_json::json!({"edit": diagnostic.edit, "message": diagnostic.kind.to_string()})
            }).collect::<Vec<_>>()}),
            )
        })?;
    let mut view = current.view(environment.functions());
    view.definition = None;
    view.not_admitted = None;
    Ok(Json(PlanResponse {
        version: current.version,
        transaction,
        preview: view,
        correspondence: applied.correspondence,
    }))
}

fn check_version(sent: u64, current: u64) -> Result<(), ErrorResponse> {
    if sent == current {
        Ok(())
    } else {
        Err(ErrorResponse::version_conflict(sent, current))
    }
}
