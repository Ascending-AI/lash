//! Throwaway HTTP adapter for the authoring seam experiment.
//! The planner knows no HTTP, revisions, template catalog or publication policy.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lash::workflow::WorkflowImport;
use lash::workflow::document::{EffectName, Expr, Name, Type};
use lash::workflow::edit::{Correspondence, Draft, Edit, Position, Transaction};
use serde::{Deserialize, Serialize};

use crate::authoring_prototype::{Step, plan};
use crate::{AppState, ErrorResponse, WorkflowView, runtime};

#[derive(Serialize)]
pub(crate) struct Template {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    entry: &'static str,
    source: &'static str,
}

pub(crate) async fn templates() -> Json<Vec<Template>> {
    Json(vec![
        Template {
            id: "inbox",
            name: "Inbox review",
            description: "A TypeScript workflow with deterministic email and display effects.",
            entry: "demo",
            source: r#"export async function demo() {
  const greeting = "Inbox review started";
  await display.show_message({ text: greeting });
  const emails = await gmail.list_recent({ count: 3 });
  await display.set_status({ key: "inbox", value: "Reviewed three mock emails" });
  return emails;
}
"#,
        },
        Template {
            id: "branch",
            name: "Conditional notification",
            description: "A nested block exercises insertion without flattening control flow.",
            entry: "demo",
            source: r#"export async function demo() {
  const notify = true;
  if (notify) {
    await display.show_message({ text: "Inside the notification branch" });
  } else {
    await display.show_message({ text: "Notifications disabled" });
  }
  return null;
}
"#,
        },
    ])
}

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
    let environment =
        runtime::environment().map_err(|error| ErrorResponse::invalid("environment", error))?;
    let imported = state
        .core
        .host_artifacts()
        .import_typescript_workflow(&request.source, &environment)
        .await
        .map_err(|error| ErrorResponse::invalid("source_import", error))?;
    let (document, annotations) = match imported {
        WorkflowImport::Imported {
            document,
            annotations,
        } => (document, annotations),
        WorkflowImport::Refused(diagnostics) => {
            return Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "source_refused",
                "TypeScript import was refused; the current document is unchanged",
                serde_json::json!({"diagnostics": diagnostics.iter().map(|diagnostic| {
                    serde_json::json!({
                        "message": diagnostic.message,
                        "span": diagnostic.span.map(|span| serde_json::json!({"start":span.start,"end":span.end})),
                    })
                }).collect::<Vec<_>>()}),
            ));
        }
        WorkflowImport::Admission(refusal) => {
            return Err(ErrorResponse::invalid("source_admission", refusal));
        }
        other => {
            return Err(ErrorResponse::invalid(
                "source_unsupported",
                format!("{other:?}"),
            ));
        }
    };
    let signature = document
        .document()
        .entries
        .get(&request.entry)
        .ok_or_else(|| ErrorResponse::invalid("entry", "Select an exported workflow function"))?;
    if !signature.params.is_empty() {
        return Err(ErrorResponse::invalid(
            "entry",
            "This prototype runs entries without parameters",
        ));
    }
    let draft = Draft::open(document.document().clone(), Some(annotations))
        .map_err(|error| ErrorResponse::invalid("document", error))?;
    let checked = state.environment().await?;
    let (saved, _) = state.install(draft, request.entry).await;
    Ok(Json(saved.view(checked.functions())))
}

/// The example owns this JSON contract. It is not a Lash SDK wire format.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StepInput {
    Direct(Edit),
    Effect {
        at: Position,
        effect: EffectName,
        args: Vec<Expr>,
        result: Type,
        binding: Name,
    },
}

impl From<StepInput> for Step {
    fn from(value: StepInput) -> Self {
        match value {
            StepInput::Direct(edit) => Self::Direct(edit),
            StepInput::Effect {
                at,
                effect,
                args,
                result,
                binding,
            } => Self::Effect {
                at,
                effect,
                args,
                result,
                binding,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRequest {
    version: u64,
    steps: Vec<StepInput>,
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
    let transaction = plan(
        &current.draft,
        request.steps.into_iter().map(Step::from).collect(),
    );
    let applied = current
        .draft
        .apply(&transaction, &environment.checker())
        .map_err(|refusal| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "plan_refused",
                "The batch was refused; the current document is unchanged",
                serde_json::json!({"diagnostics": refusal.diagnostics.iter().map(|diagnostic| {
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
