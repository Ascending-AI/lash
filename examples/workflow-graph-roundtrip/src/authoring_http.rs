//! Example-owned HTTP adapter for Lash's public authoring API.
//! The closed authoring vocabulary knows no HTTP, revisions, catalog or publication policy.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lash::schema::Name;
use lash::workflow::WorkflowImport;
use lash::workflow::authoring::Operation;
use lash::workflow::document::{Captions, Correspondence, Document, SourceMap};
use lash::workflow::edit::{Draft, EditDiagnosticKind, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    AppState, CaptionWorkflowRequest, Dialect, ErrorResponse, Origin, WorkflowView, runtime,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRequest {
    version: u64,
    source: String,
    entry: Name,
    #[serde(default)]
    dialect: Dialect,
    format: Option<lash::dialect::SourceFormat>,
}

pub(crate) async fn import_source(
    State(state): State<AppState>,
    Json(request): Json<SourceRequest>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    check_version(request.version, current.version)?;
    let (document, map, captions, packages) = if let Some(format) = request.format {
        let packages = match current.namespace.clone() {
            Some(packages) => packages,
            None => runtime::namespace(&state.core, &state.namespace_pin)
                .await
                .map_err(|error| ErrorResponse::invalid("namespace", error))?,
        };
        let read = state
            .core
            .host_artifacts()
            .import_exported_workflow(
                &request.source,
                format,
                current.draft.document(),
                &packages.request,
            )
            .await
            .map_err(|error| ErrorResponse::invalid("source_read", error))?;
        match read {
            WorkflowImport::Read { document, captions } => (
                document
                    .to_document()
                    .map_err(|error| ErrorResponse::invalid("document_bytes", error))?,
                None,
                captions,
                packages,
            ),
            other => {
                return Err(ErrorResponse::invalid(
                    "source_read_refused",
                    format!("{other:?}"),
                ));
            }
        }
    } else {
        import(
            &state.core,
            &state.namespace_pin,
            request.dialect,
            &request.source,
        )
        .await?
    };
    if !document.exports.contains_key(&request.entry) {
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
            if let Some(format) = request.format {
                Origin::Read {
                    dialect: if format.language()
                        == lash::workflow::document::SourceLanguage::Python
                    {
                        Dialect::Python
                    } else {
                        Dialect::TypeScript
                    },
                    captions,
                }
            } else {
                Origin::Imported {
                    packages: Box::new(packages),
                    dialect: request.dialect,
                    text: request.source,
                    map,
                    captions,
                }
            },
        )
        .await;
    Ok(Json(
        saved
            .view(&state.core, &state.workers, checked.functions())
            .await,
    ))
}

/// Lowers authored source in `dialect` through lash's workflow import and
/// admits it against the run environment, as written: the host rewrites
/// nothing. The captions are the ones the source spells.
pub(crate) async fn import(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    dialect: Dialect,
    source: &str,
) -> Result<(Document, Option<SourceMap>, Captions, runtime::Namespace), ErrorResponse> {
    let namespace = runtime::namespace(core, pin)
        .await
        .map_err(|error| ErrorResponse::invalid("namespace", error))?;
    let imported = core
        .host_artifacts()
        .import_workflow(dialect.language(), source, &namespace.request, None)
        .await
        .map_err(|error| ErrorResponse::invalid("source_import", error))?;
    match imported {
        WorkflowImport::Imported {
            document,
            source_map,
            captions,
            ..
        } => Ok((
            document
                .to_document()
                .map_err(|error| ErrorResponse::invalid("document_bytes", error))?,
            Some(source_map),
            captions,
            namespace,
        )),
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
    // The preview shows the captions the edit would keep, and marks.
    (current.captions, current.review) =
        current.rekeyed(&applied.correspondence, current.draft.document())?;
    // A preview is an unpublished edit and must not read the old definition or map.
    current.published = Err(std::sync::Arc::new(runtime::RunError::Invalid(
        "preview".into(),
    )));
    if let Some(authored) = &mut current.authored {
        authored.source_map_id = None;
        authored.source_map = None;
    }
    let mut view = current
        .view(&state.core, &state.workers, environment.functions())
        .await;
    view.definition = None;
    view.not_admitted = None;
    Ok(Json(PlanResponse {
        version: current.version,
        transaction,
        preview: view,
        correspondence: applied.correspondence,
    }))
}

/// Saves the request's captions as the next version of the same document.
/// The host builds the new `Captions` value; lash checks every site and
/// text. Nothing about the document, its draft or its publication changes.
pub(crate) async fn caption(
    State(state): State<AppState>,
    request: Result<Json<CaptionWorkflowRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    // A caption's text is checked as it is decoded (`CaptionRefusal`).
    let Json(request) =
        request.map_err(|error| ErrorResponse::invalid("caption_refused", error.body_text()))?;
    let environment = state.environment().await?;
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    check_version(request.version, current.version)?;
    let captions = Captions::new(current.draft.document(), request.entries)
        .map_err(|refusal| ErrorResponse::invalid("caption_refused", refusal))?;
    let review = current
        .review
        .iter()
        .filter(|site| !request.reviewed.contains(site) && captions.at(site).is_some())
        .cloned()
        .collect();
    let (saved, _) = state
        .install(
            current.draft.clone(),
            current.entry.clone(),
            Origin::Edited { captions, review },
        )
        .await;
    Ok(Json(
        saved
            .view(&state.core, &state.workers, environment.functions())
            .await,
    ))
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
