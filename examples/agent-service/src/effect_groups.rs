#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]
#![cfg(feature = "restate")]

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use lash::CancellationToken;
use lash::runtime::{
    GroupExecutors, GroupWakePolicy, LoserPolicy, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeInvocation, RuntimeScope,
};
use lash_restate::{
    EffectGroupReadRankRequest, EffectGroupReadRankResponse, EffectGroupSettlementTerminal,
    RestateEffectGroupRetryPolicy, RestateEffectGroupServices, RestateIngressClient,
    RestateRuntimeEffectController,
};
use serde::{Deserialize, Serialize};

use crate::state::{AgentServiceDurability, AppError, AppResult, AppStateData};

const CHILD_DURATIONS_MS: [u64; 3] = [25, 60_000, 60_000];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct EffectGroupRunRequest {
    run_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct EffectGroupWorkflowResult {
    group_key: String,
    first_position: usize,
    first_sequence: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectGroupRunTerminal {
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct EffectGroupRankWitness {
    pub(crate) rank: u64,
    pub(crate) position: usize,
    pub(crate) sequence: u64,
    pub(crate) terminal: EffectGroupRunTerminal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct EffectGroupRunReport {
    pub(crate) run_id: String,
    pub(crate) group_key: String,
    pub(crate) child_count: usize,
    pub(crate) group_admitted: bool,
    pub(crate) children_dispatched: bool,
    pub(crate) first_settlement_rank: u64,
    pub(crate) first_settlement_position: usize,
    pub(crate) settlements: Vec<EffectGroupRankWitness>,
    pub(crate) cancelled_losers: usize,
    pub(crate) group_terminal: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentServiceEffectGroupExecutors;

impl GroupExecutors for AgentServiceEffectGroupExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        matches!(&envelope.command, RuntimeEffectCommand::Sleep { .. })
            .then(|| RuntimeEffectLocalExecutor::sleep(CancellationToken::new()))
    }
}

#[restate_sdk::workflow]
pub(crate) trait AgentServiceEffectGroupWorkflow {
    async fn run(
        request: restate_sdk::serde::Json<EffectGroupRunRequest>,
    ) -> restate_sdk::errors::HandlerResult<restate_sdk::serde::Json<EffectGroupWorkflowResult>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AgentServiceEffectGroupWorkflowImpl;

impl AgentServiceEffectGroupWorkflow for AgentServiceEffectGroupWorkflowImpl {
    async fn run(
        &self,
        ctx: restate_sdk::prelude::WorkflowContext<'_>,
        restate_sdk::serde::Json(request): restate_sdk::serde::Json<EffectGroupRunRequest>,
    ) -> restate_sdk::errors::HandlerResult<restate_sdk::serde::Json<EffectGroupWorkflowResult>>
    {
        validate_run_id(&request.run_id).map_err(restate_sdk::errors::TerminalError::new)?;
        let group = effect_group(&request.run_id)
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        let group_key = group.group_key().to_string();
        let controller = RestateRuntimeEffectController::new(ctx);
        let mut handle = controller
            .open_effect_group(group)
            .await
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        let first = controller
            .await_next_settlement(&mut handle, CancellationToken::new())
            .await
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        if !matches!(first.outcome, Ok(RuntimeEffectOutcome::Sleep)) {
            return Err(restate_sdk::errors::TerminalError::new(format!(
                "effect group {group_key} first settlement was not a completed sleep"
            ))
            .into());
        }
        controller
            .close_effect_group(handle, LoserPolicy::Cancel)
            .await
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        Ok(restate_sdk::serde::Json(EffectGroupWorkflowResult {
            group_key,
            first_position: first.position,
            first_sequence: first.sequence,
        }))
    }
}

pub(crate) fn effect_group_services(ingress_url: impl Into<String>) -> RestateEffectGroupServices {
    RestateEffectGroupServices::new(
        Arc::new(AgentServiceEffectGroupExecutors),
        RestateIngressClient::new(ingress_url.into()),
        RestateEffectGroupRetryPolicy::infinite(),
    )
}

pub(crate) async fn run_effect_group(
    State(state): State<AppStateData>,
    Json(request): Json<EffectGroupRunRequest>,
) -> AppResult<Json<EffectGroupRunReport>> {
    validate_state_and_run_id(&state, &request.run_id)?;
    let ingress = RestateIngressClient::new(
        state
            .restate_ingress_url()
            .expect("Restate durability validates the ingress URL")
            .to_string(),
    );
    ensure_group_is_new(&ingress, &request.run_id).await?;
    let workflow: EffectGroupWorkflowResult = ingress
        .call_workflow_json(
            "AgentServiceEffectGroupWorkflow",
            &request.run_id,
            "run",
            &request,
        )
        .await
        .map_err(|error| AppError::internal(format!("run Restate effect group: {error}")))?;
    let report = read_effect_group_report(&ingress, request.run_id).await?;
    let first = report
        .settlements
        .first()
        .expect("a terminal three-child report has rank one");
    if workflow.group_key != report.group_key
        || workflow.first_position != first.position
        || workflow.first_sequence != first.sequence
    {
        return Err(AppError::internal(format!(
            "effect-group workflow result disagrees with its durable ranks: workflow={workflow:?}, report={report:?}"
        )));
    }
    Ok(Json(report))
}

pub(crate) async fn get_effect_group(
    State(state): State<AppStateData>,
    AxumPath(run_id): AxumPath<String>,
) -> AppResult<Json<EffectGroupRunReport>> {
    validate_state_and_run_id(&state, &run_id)?;
    let ingress = RestateIngressClient::new(
        state
            .restate_ingress_url()
            .expect("Restate durability validates the ingress URL")
            .to_string(),
    );
    read_effect_group_report(&ingress, run_id).await.map(Json)
}

fn validate_state_and_run_id(state: &AppStateData, run_id: &str) -> AppResult<()> {
    if state.durability() != AgentServiceDurability::Restate {
        return Err(AppError::bad_request(
            "effect groups require AGENT_SERVICE_DURABILITY=restate",
        ));
    }
    validate_run_id(run_id).map_err(AppError::bad_request)
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

async fn ensure_group_is_new(ingress: &RestateIngressClient, run_id: &str) -> AppResult<()> {
    let response: EffectGroupReadRankResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key(run_id),
            "read_rank",
            &EffectGroupReadRankRequest { rank: 1 },
        )
        .await
        .map_err(|error| AppError::internal(format!("probe effect group rank one: {error}")))?;
    if matches!(response, EffectGroupReadRankResponse::UnknownGroup) {
        return Ok(());
    }
    Err(AppError::bad_request(format!(
        "effect-group run_id `{run_id}` already exists"
    )))
}

async fn read_effect_group_report(
    ingress: &RestateIngressClient,
    run_id: String,
) -> AppResult<EffectGroupRunReport> {
    let group_key = group_key(&run_id);
    let mut settlements = Vec::with_capacity(CHILD_DURATIONS_MS.len());
    for rank in 1..=CHILD_DURATIONS_MS.len() as u64 {
        let response: EffectGroupReadRankResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "read_rank",
                &EffectGroupReadRankRequest { rank },
            )
            .await
            .map_err(|error| {
                AppError::internal(format!(
                    "read effect group {group_key} rank {rank}: {error}"
                ))
            })?;
        let settlement = match response {
            EffectGroupReadRankResponse::Settled { settlement } => settlement,
            EffectGroupReadRankResponse::NotSettled => {
                return Err(AppError::internal(format!(
                    "effect group {group_key} is not terminal: rank {rank} has not settled"
                )));
            }
            EffectGroupReadRankResponse::UnknownGroup => {
                return Err(AppError::bad_request(format!(
                    "effect-group run_id `{run_id}` does not exist"
                )));
            }
            EffectGroupReadRankResponse::Retired => {
                return Err(AppError::internal(format!(
                    "effect group {group_key} was retired before its report was read"
                )));
            }
        };
        let terminal = match settlement.terminal {
            EffectGroupSettlementTerminal::StoredPayload => EffectGroupRunTerminal::Completed,
            EffectGroupSettlementTerminal::Cancelled => EffectGroupRunTerminal::Cancelled,
            EffectGroupSettlementTerminal::Failed { error } => {
                return Err(AppError::internal(format!(
                    "effect group {group_key} rank {rank} failed: {error}"
                )));
            }
        };
        settlements.push(EffectGroupRankWitness {
            rank,
            position: settlement.position,
            sequence: settlement.sequence,
            terminal,
        });
    }

    let first = settlements
        .first()
        .expect("the worked group always has three children");
    let mut positions = settlements
        .iter()
        .map(|settlement| settlement.position)
        .collect::<Vec<_>>();
    positions.sort_unstable();
    if positions != (0..CHILD_DURATIONS_MS.len()).collect::<Vec<_>>() {
        return Err(AppError::internal(format!(
            "effect group {group_key} did not settle each child position exactly once"
        )));
    }
    if !settlements
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence)
    {
        return Err(AppError::internal(format!(
            "effect group {group_key} settlement sequences were not strictly increasing"
        )));
    }
    if first.terminal != EffectGroupRunTerminal::Completed {
        return Err(AppError::internal(format!(
            "effect group {group_key} rank one was not the completed winner"
        )));
    }
    let cancelled_losers = settlements
        .iter()
        .skip(1)
        .filter(|settlement| settlement.terminal == EffectGroupRunTerminal::Cancelled)
        .count();
    if cancelled_losers != CHILD_DURATIONS_MS.len() - 1 {
        return Err(AppError::internal(format!(
            "effect group {group_key} did not close with both losers cancelled"
        )));
    }

    Ok(EffectGroupRunReport {
        run_id,
        group_key,
        child_count: CHILD_DURATIONS_MS.len(),
        group_admitted: true,
        children_dispatched: true,
        first_settlement_rank: 1,
        first_settlement_position: first.position,
        settlements,
        cancelled_losers,
        group_terminal: true,
    })
}

fn effect_group(
    run_id: &str,
) -> Result<RuntimeEffectGroup, lash::runtime::RuntimeEffectControllerError> {
    let group_key = group_key(run_id);
    let scope = RuntimeScope::new(format!("agent-service-effect-group:{run_id}"));
    let children = CHILD_DURATIONS_MS
        .iter()
        .enumerate()
        .map(|(position, duration_ms)| {
            RuntimeEffectEnvelope::new(
                RuntimeInvocation::effect(
                    scope.clone(),
                    format!("sleep-{position}"),
                    RuntimeEffectKind::Sleep,
                    child_replay_key(&group_key, position),
                ),
                RuntimeEffectCommand::Sleep {
                    duration_ms: *duration_ms,
                },
            )
        })
        .collect();
    RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            scope,
            "effect-group",
            RuntimeEffectKind::Sleep,
            format!("{group_key}:group"),
        ),
        group_key,
        children,
        GroupWakePolicy::First,
        LoserPolicy::Cancel,
    )
}

fn group_key(run_id: &str) -> String {
    format!("agent-service:effect-group:{run_id}")
}

fn child_replay_key(group_key: &str, position: usize) -> String {
    format!("{group_key}:child:{position}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worked_group_is_a_three_child_first_settlement_deadline() {
        let group = effect_group("shape-test").expect("worked group assembles");
        assert_eq!(group.children().len(), 3);
        assert_eq!(group.wake(), GroupWakePolicy::First);
        assert_eq!(group.loser_disposition(), LoserPolicy::Cancel);
        for (position, child) in group.children().iter().enumerate() {
            let membership = child.group.as_deref().expect("child is stamped");
            assert_eq!(membership.position, position);
            assert_eq!(membership.group_key, group.group_key());
            assert_eq!(
                child.invocation.replay_key(),
                Some(child_replay_key(group.group_key(), position).as_str())
            );
        }
    }

    #[test]
    fn deployment_resolver_routes_only_the_worked_sleep_children() {
        let group = effect_group("routing-test").expect("worked group assembles");
        let resolver = AgentServiceEffectGroupExecutors;
        assert!(
            group
                .children()
                .iter()
                .all(|child| resolver.executor_for(child).is_some())
        );

        let unsupported = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::new("routing-test"),
                "unsupported",
                RuntimeEffectKind::LanguageRuntimeValue,
                "routing-test:unsupported",
            ),
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "unsupported".to_string(),
            },
        );
        assert!(resolver.executor_for(&unsupported).is_none());
    }

    #[test]
    fn run_ids_are_safe_restate_workflow_keys() {
        assert!(validate_run_id("cov1_witness-42").is_ok());
        assert!(validate_run_id("").is_err());
        assert!(validate_run_id("contains/slash").is_err());
        assert!(validate_run_id(&"x".repeat(97)).is_err());
    }
}
