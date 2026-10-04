use axum::Json;
use axum::extract::{Path as AxumPath, State};
use lash::durability::EffectOpener;
use lash::restate::RestateRuntimeEffectController;
use lash::restate::restate_sdk;
use lash::runtime::{
    AdmittedScope, AggregateConsumer, AggregateLeaf, AggregatePlan, RunAggregateOutcome,
    RunCoordinator, RunLifecycle, ScopedEffectController, SegmentOrdinal, SystemClock,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::state::{AppError, AppResult, AppStateData};

const DURATIONS_MS: [u64; 3] = [25, 60_000, 60_000];
const REPORT_KEY: &str = "aggregate-report";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AggregateRunRequest {
    run_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AggregateRunReport {
    pub(crate) run_id: String,
    pub(crate) winner: u32,
    pub(crate) completed: Vec<u32>,
    pub(crate) cancelled: Vec<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentServiceAggregateWorkflowImpl {
    pub(crate) build_generation: lash::BuildGeneration,
}

#[restate_sdk::workflow(name = "AgentServiceAggregateWorkflow")]
impl AgentServiceAggregateWorkflowImpl {
    #[restate_sdk::handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        restate_sdk::serde::Json(request): restate_sdk::serde::Json<AggregateRunRequest>,
    ) -> HandlerResult<restate_sdk::serde::Json<AggregateRunReport>> {
        validate_run_id(&request.run_id).map_err(TerminalError::new)?;
        let authority_id = lash::restate::RestateAuthorityId::new(
            std::env::var("RESTATE_AUTHORITY_ID")
                .map_err(|_| TerminalError::new("RESTATE_AUTHORITY_ID is required"))?,
        )
        .map_err(TerminalError::from_error)?;
        let controller =
            RestateRuntimeEffectController::new(ctx, authority_id, self.build_generation.clone());
        let scope =
            AdmittedScope::session_operation("agent-service:timers", request.run_id.clone());
        let scoped = ScopedEffectController::borrowed(&controller, scope)
            .map_err(TerminalError::from_error)?;
        let report = run_timers(&scoped, request.run_id)
            .await
            .map_err(TerminalError::from_error)?;
        controller
            .context()
            .set(REPORT_KEY, restate_sdk::serde::Json(report.clone()));
        Ok(restate_sdk::serde::Json(report))
    }

    #[restate_sdk::handler]
    async fn report(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> HandlerResult<restate_sdk::serde::Json<Option<AggregateRunReport>>> {
        Ok(restate_sdk::serde::Json(
            ctx.get::<restate_sdk::serde::Json<AggregateRunReport>>(REPORT_KEY)
                .await?
                .map(|value| value.0),
        ))
    }
}

async fn run_timers(
    scoped: &ScopedEffectController<'_>,
    run_id: String,
) -> Result<AggregateRunReport, lash::runtime::SingletonRunError> {
    let plan = AggregatePlan {
        key: run_id.clone(),
        leaves: DURATIONS_MS
            .into_iter()
            .map(|duration_ms| AggregateLeaf::Timer { duration_ms })
            .collect(),
        operands: vec![0, 1, 2],
    };
    let owner = EffectOpener::session_operation("agent-service:timers", run_id.clone());
    let mut run = RunCoordinator::open(scoped, owner, SegmentOrdinal(0), Vec::new());
    run.admit_aggregate(&plan, &SystemClock).await?;
    let selected = run
        .consume_aggregate(&plan.key, AggregateConsumer::Race)
        .await?;
    let RunAggregateOutcome::Selected {
        operand: winner,
        fulfilled: true,
        ..
    } = selected
    else {
        return Err(lash::runtime::RuntimeEffectControllerError::new(
            lash::runtime::RuntimeErrorCode::EffectReplayDivergence,
            "timer race has no fulfilled winner",
        )
        .into());
    };
    // Only the logical owner closes. The recorded Closing cancels timers still
    // pending; an early race result alone keeps every timer alive.
    run.close().await?;
    assert_eq!(run.lifecycle(), RunLifecycle::Settled);
    let completed: Vec<_> = run
        .records()
        .iter()
        .flat_map(|record| &record.events)
        .filter_map(|event| match event {
            lash::runtime::run_event::RunEvent::TimerElapsed { aggregate, leaf }
                if aggregate == &plan.key =>
            {
                Some(*leaf)
            }
            _ => None,
        })
        .collect();
    let cancelled = (0..DURATIONS_MS.len() as u32)
        .filter(|leaf| !completed.contains(leaf))
        .collect();
    Ok(AggregateRunReport {
        run_id,
        winner,
        completed,
        cancelled,
    })
}

pub(crate) async fn run_aggregate(
    State(state): State<AppStateData>,
    Json(request): Json<AggregateRunRequest>,
) -> AppResult<Json<AggregateRunReport>> {
    validate_run_id(&request.run_id).map_err(AppError::bad_request)?;
    let ingress = state.restate_ingress();
    let existing: Option<AggregateRunReport> = ingress
        .call_workflow_json(
            "AgentServiceAggregateWorkflow",
            &request.run_id,
            "report",
            &(),
        )
        .await
        .map_err(|error| AppError::internal(format!("read aggregate report: {error}")))?;
    if existing.is_some() {
        return Err(AppError::bad_request(format!(
            "aggregate run_id `{}` already exists",
            request.run_id
        )));
    }
    ingress
        .call_workflow_json(
            "AgentServiceAggregateWorkflow",
            &request.run_id,
            "run",
            &request,
        )
        .await
        .map(Json)
        .map_err(|error| AppError::internal(format!("run aggregate: {error}")))
}

pub(crate) async fn get_aggregate(
    State(state): State<AppStateData>,
    AxumPath(run_id): AxumPath<String>,
) -> AppResult<Json<AggregateRunReport>> {
    validate_run_id(&run_id).map_err(AppError::bad_request)?;
    let report: Option<AggregateRunReport> = state
        .restate_ingress()
        .call_workflow_json("AgentServiceAggregateWorkflow", &run_id, "report", &())
        .await
        .map_err(|error| AppError::internal(format!("read aggregate report: {error}")))?;
    report
        .map(Json)
        .ok_or_else(|| AppError::bad_request(format!("aggregate run_id `{run_id}` does not exist")))
}

fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.is_empty() || run_id.len() > 96 {
        return Err("run_id must contain 1..=96 characters".to_string());
    }
    if !run_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("run_id may contain only ASCII letters, digits, '-' and '_'".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_safe_restate_workflow_keys() {
        assert!(validate_run_id("cov1_witness-42").is_ok());
        assert!(validate_run_id("").is_err());
        assert!(validate_run_id("contains/slash").is_err());
        assert!(validate_run_id(&"x".repeat(97)).is_err());
    }
}
