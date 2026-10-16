//! Example-owned HTTP adapter for Lash's public authoring API.
//! The closed authoring vocabulary knows no HTTP, revisions, catalog or publication policy.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lash::workflow::WorkflowImport;
use lash::workflow::authoring::Operation;
use lash::workflow::document::{Correspondence, Document, Name};
use lash::workflow::edit::{Draft, EditDiagnosticKind, Transaction};
use serde::{Deserialize, Serialize};

use crate::{AppState, Dialect, ErrorResponse, Origin, WorkflowView, runtime};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRequest {
    version: u64,
    source: String,
    entry: Name,
    #[serde(default)]
    dialect: Dialect,
}

pub(crate) async fn import_source(
    State(state): State<AppState>,
    Json(request): Json<SourceRequest>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    check_version(request.version, current.version)?;
    let document = import(&state.core, request.dialect, &request.source).await?;
    if !document.entries.contains_key(&request.entry) {
        return Err(ErrorResponse::invalid(
            "entry",
            "Select an exported workflow function",
        ));
    }
    let draft = Draft::open(document).map_err(|error| ErrorResponse::invalid("document", error))?;
    let checked = state.environment().await?;
    let (saved, _) = state
        .install(
            draft,
            request.entry,
            Origin::Imported {
                dialect: request.dialect,
                text: request.source,
            },
        )
        .await;
    Ok(Json(saved.view(checked.functions())))
}

/// Lowers authored source in `dialect` through lash's workflow import and
/// admits it against the run environment, as written: the host rewrites
/// nothing.
pub(crate) async fn import(
    core: &lash::LashCore,
    dialect: Dialect,
    source: &str,
) -> Result<Document, ErrorResponse> {
    let environment =
        runtime::environment().map_err(|error| ErrorResponse::invalid("environment", error))?;
    let imported = core
        .host_artifacts()
        .import_workflow(dialect.language(), source, &environment, None)
        .await
        .map_err(|error| ErrorResponse::invalid("source_import", error))?;
    match imported {
        WorkflowImport::Imported { document, .. } => document
            .to_document()
            .map_err(|error| ErrorResponse::invalid("document_bytes", error)),
        WorkflowImport::Refused(diagnostics) => Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "source_refused",
            "the import was refused; the current document is unchanged",
            serde_json::json!({"diagnostics": diagnostics.iter().map(|diagnostic| {
                serde_json::json!({
                    "code": diagnostic.code,
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
    request: Result<Json<PlanRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<PlanResponse>, ErrorResponse> {
    let Json(request) = request.map_err(unsupported)?;
    let environment = state.environment().await?;
    let mut current = state.current();
    check_version(request.version, current.version)?;
    let transaction = Transaction {
        base: current.draft.identity(),
        edits: request.operations,
    };
    let applied = current
        .draft
        .apply(&transaction, &environment.checker())
        .map_err(|refusal| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "plan_refused",
                "The batch was refused; the current document is unchanged",
                serde_json::json!({"transaction": &transaction, "diagnostics": refusal.diagnostics.iter().map(|diagnostic| {
                serde_json::json!({"edit": diagnostic.edit, "message": diagnostic.kind.to_string(), "cause": diagnostic.kind, "location": diagnostic.location})
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

/// Unknown or malformed canvas operations have one typed transport cause.
pub(crate) fn unsupported(error: axum::extract::rejection::JsonRejection) -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_canvas_edit",
        error.body_text(),
        serde_json::json!({"cause": EditDiagnosticKind::UnsupportedCanvasEdit}),
    )
}
