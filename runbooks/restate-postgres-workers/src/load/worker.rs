//! The worker half of the load workload: one Restate workflow whose handler
//! runs one [`LoadRequest`] through lash's public API.

use super::{
    CRON_SETUP_MARKER, CRON_SOURCE_TYPE, CancelOutcome, CronSetupReport, CronTickReport,
    DeleteReport, DeletionOutcome, HostProcessReport, InputOutcome, LoadContext, LoadRequest,
    LoadResponse, QUEUED_MARKER, QueuedReport, TURN_MARKER, TurnReport, WORKLOAD_MARKER,
    turn_id_for,
};
use crate::{create_or_open_session, turn_handler_error};
use anyhow::{Context, Result};
use lash::restate::RestateWait;
use lash::runtime::AwaitEventResolver as _;
use lash::{SessionId, TurnInput};
use lash_perf::workload::{Generator, ProcessPlan, QueuedInputPlan, TurnPlan};
use lash_restate::RestateRuntimeEffectController;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::prelude::WorkflowContext;
use restate_sdk::serde::Json;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

// The struct-based service API has no workflow form yet; lash's own services
// and the e2e turn workflow bind the same way.
#[allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]
mod service {
    use super::{LoadRequest, LoadResponse};
    use restate_sdk::prelude::*;

    #[restate_sdk::workflow]
    pub trait E2eLoadWorkflow {
        async fn run(request: Json<LoadRequest>) -> HandlerResult<Json<LoadResponse>>;
    }
}
pub use service::E2eLoadWorkflow;

/// How long a delete waits for a closing session's owed physical delete.
const CLOSING_DELETE_WAIT: Duration = Duration::from_secs(120);

type Controller<'ctx> = RestateRuntimeEffectController<'ctx, WorkflowContext<'ctx>>;

fn terminal(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn terminal_chain(error: anyhow::Error) -> HandlerError {
    TerminalError::new(format!("{error:#}")).into()
}

/// A worker's load workflow, over the worker's one core.
#[derive(Clone)]
pub struct LoadWorker {
    worker_id: String,
    core: lash::LashCore,
    witness: PgPool,
    load: LoadContext,
    restate_ingress_url: String,
    restate_authority_id: lash_restate::RestateAuthorityId,
    model: lash::ModelSpec,
    administration: Arc<tokio::sync::OnceCell<lash_restate::RestateSessionAdministration>>,
}

pub struct LoadWorkerConfig {
    pub worker_id: String,
    pub core: lash::LashCore,
    pub witness: PgPool,
    pub load: LoadContext,
    pub restate_ingress_url: String,
    pub restate_authority_id: lash_restate::RestateAuthorityId,
    pub model: lash::ModelSpec,
}

impl LoadWorker {
    pub fn new(config: LoadWorkerConfig) -> Self {
        Self {
            worker_id: config.worker_id,
            core: config.core,
            witness: config.witness,
            load: config.load,
            restate_ingress_url: config.restate_ingress_url,
            restate_authority_id: config.restate_authority_id,
            model: config.model,
            administration: Arc::new(tokio::sync::OnceCell::new()),
        }
    }
}

impl E2eLoadWorkflow for LoadWorker {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<LoadRequest>,
    ) -> HandlerResult<Json<LoadResponse>> {
        let response = match request {
            LoadRequest::Turn {
                workload_sha256,
                run,
                actor,
                ordinal,
                session_id,
            } => {
                self.load
                    .require_workload(&workload_sha256)
                    .map_err(terminal_chain)?;
                let generator = self.load.generator(&run).map_err(terminal_chain)?;
                let plan = generator.plan(actor, ordinal).map_err(terminal_chain)?;
                let controller =
                    RestateRuntimeEffectController::new(ctx, self.restate_authority_id.clone());
                LoadResponse::Turn(
                    Box::pin(self.turn(&controller, &generator, &plan, session_id)).await?,
                )
            }
            LoadRequest::CronSetup {
                workload_sha256,
                run,
                session_id,
            } => {
                self.load
                    .require_workload(&workload_sha256)
                    .map_err(terminal_chain)?;
                let controller =
                    RestateRuntimeEffectController::new(ctx, self.restate_authority_id.clone());
                LoadResponse::CronSetup(
                    Box::pin(self.cron_setup(&controller, &run, session_id)).await?,
                )
            }
            LoadRequest::CronTick {
                workload_sha256,
                run,
                subscription,
                tick,
            } => {
                self.load
                    .require_workload(&workload_sha256)
                    .map_err(terminal_chain)?;
                let generator = self.load.generator(&run).map_err(terminal_chain)?;
                let controller =
                    RestateRuntimeEffectController::new(ctx, self.restate_authority_id.clone());
                LoadResponse::CronTick(
                    Box::pin(self.cron_tick(&controller, &generator, subscription, tick)).await?,
                )
            }
            LoadRequest::DeleteSession { session_id, .. } => {
                LoadResponse::DeleteSession(Box::pin(self.delete_session(ctx, session_id)).await?)
            }
        };
        Ok(Json(response))
    }
}

impl LoadWorker {
    /// Primary turn `plan`: accept it, feed its queued inputs while it runs,
    /// cancel what the plan cancels, await every root, then run its host
    /// process starts.
    async fn turn(
        &self,
        controller: &Controller<'_>,
        generator: &Generator<'_>,
        plan: &TurnPlan,
        session_id: String,
    ) -> HandlerResult<TurnReport> {
        let ctx = controller.context();
        let id = &plan.operation;
        let operation = id.key();
        let session = create_or_open_session(&self.core, session_id.clone()).await?;
        let input = generator
            .record(id.actor, id.ordinal, "input", plan.input_bytes)
            .map_err(terminal_chain)?;
        let prompt = generator.text(id.actor, id.ordinal, "prompt", plan.prompt_bytes);
        let layer = lash::prompt::PromptLayer::new().with_contribution(
            lash::prompt::PromptContribution::new(
                lash::prompt::PromptSlot::ProjectInstructions,
                "Synthetic load context",
                prompt,
            ),
        );
        let main = session
            .send(TurnInput::text(format!(
                "Run the synthetic load turn. {TURN_MARKER}{operation} {WORKLOAD_MARKER}{}\n{input}",
                self.load.sha256()
            )))
            .id(turn_id_for(&operation))
            .prompt_layer(layer)
            .accept_restate(ctx)
            .await?;
        // A queued input or a cancel is meant for a running root: wait until
        // the provider has been asked for this turn's cell.
        let needs_running_root = plan.cancel
            || plan
                .queued_inputs
                .iter()
                .any(|queued| queued.during_active_turn);
        if needs_running_root {
            wait_for_provider_receipt(&self.witness, &operation)
                .await
                .map_err(terminal_chain)?;
        }
        let mut queued = Vec::new();
        for input in plan
            .queued_inputs
            .iter()
            .filter(|input| input.during_active_turn)
        {
            queued.push((input, self.send_queued(ctx, &session, input).await?, None));
        }
        let cancel = if plan.cancel {
            Some(cancel_outcome(
                main.cancel()
                    .origin("lash-loadtest-driver")
                    .request_id(format!("{operation}/cancel"))
                    .await
                    .map_err(turn_handler_error)?,
            ))
        } else {
            None
        };
        for (input, handle, cancel) in &mut queued {
            *cancel = self.cancel_queued(input, handle).await?;
        }
        let outcome = input_outcome(main.outcome_restate(ctx, RestateWait::new()).await?);
        // Inputs that arrive once the session is idle run as their own roots.
        for input in plan
            .queued_inputs
            .iter()
            .filter(|input| !input.during_active_turn)
        {
            let handle = self.send_queued(ctx, &session, input).await?;
            let cancel = self.cancel_queued(input, &handle).await?;
            queued.push((input, handle, cancel));
        }
        let mut queued_reports = Vec::new();
        for (input, handle, cancel) in queued {
            queued_reports.push(QueuedReport {
                key: input.idempotency_key.clone(),
                during_active_turn: input.during_active_turn,
                cancel,
                outcome: input_outcome(handle.outcome_restate(ctx, RestateWait::new()).await?),
            });
        }
        let mut host_processes = Vec::new();
        for process in &plan.host_processes {
            host_processes.push(Box::pin(self.host_process(controller, generator, process)).await?);
        }
        Ok(TurnReport {
            worker_id: self.worker_id.clone(),
            operation,
            session_id,
            outcome,
            cancel,
            queued: queued_reports,
            host_processes,
        })
    }

    async fn send_queued(
        &self,
        ctx: &WorkflowContext<'_>,
        session: &lash::LashSession,
        input: &QueuedInputPlan,
    ) -> HandlerResult<lash::SendHandle> {
        session
            .send(TurnInput::text(format!(
                "Run the synthetic queued input. {QUEUED_MARKER}{} {WORKLOAD_MARKER}{}",
                input.idempotency_key,
                self.load.sha256()
            )))
            .id(turn_id_for(&input.idempotency_key))
            .accept_restate(ctx)
            .await
    }

    async fn cancel_queued(
        &self,
        input: &QueuedInputPlan,
        handle: &lash::SendHandle,
    ) -> HandlerResult<Option<CancelOutcome>> {
        if !input.cancel {
            return Ok(None);
        }
        let receipt = handle
            .cancel()
            .origin("lash-loadtest-driver")
            .request_id(format!("{}/cancel", input.idempotency_key))
            .await
            .map_err(turn_handler_error)?;
        Ok(Some(cancel_outcome(receipt)))
    }

    /// A host start of the plan's durable body, then its signal or cancel
    /// and its awaited terminal.
    async fn host_process(
        &self,
        controller: &Controller<'_>,
        generator: &Generator<'_>,
        process: &ProcessPlan,
    ) -> HandlerResult<HostProcessReport> {
        let key = &process.idempotency_key;
        let environment = lash::rlm::LashlangHostEnvironment::new(
            lash::rlm::LashlangHostCatalog::new(),
            lash::rlm::LashlangAbilities::all(),
        );
        let linked = lash::typescript::link(&generator.process_body(process), &environment)
            .map_err(|error| terminal(format!("link the body of `{key}`: {error:?}")))?;
        let declaration = linked
            .artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                lashlang::Declaration::Process(process) => Some(process),
                _ => None,
            })
            .ok_or_else(|| terminal(format!("the body of `{key}` declares no process")))?;
        let process_name = declaration.name.to_string();
        // A host pin keeps the module alive for the process (ADR 0113).
        let pin = lash::process::HostArtifactPin::mint();
        self.core
            .host_artifacts()
            .publish_module(&pin, &linked.artifact)
            .await
            .map_err(turn_handler_error)?;
        let mut args = serde_json::Map::new();
        args.insert("key".to_owned(), Value::String(key.clone()));
        let input = lash::process::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(&process_name)
                .ok_or_else(|| terminal(format!("the body of `{key}` has no process ref")))?
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name,
            args,
        }
        .into_process_input()
        .map_err(terminal)?;
        let environment = lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy {
                model: self.model.clone(),
                ..lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded)
            },
        );
        let request = lash_core::ProcessStartRequest::new(
            input,
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_host_start_key(key.as_bytes())
        .with_env_spec(environment)
        .with_extra_event_types(
            lash::process::lashlang_process_event_types()
                .into_iter()
                .chain(lash::process::lashlang_process_signal_event_types(
                    declaration,
                )),
        );
        let processes = self.core.processes();
        let receipt = processes
            .start(request, scoped(controller, key, "start")?)
            .await
            .map_err(turn_handler_error)?;
        let process_id = receipt.process_id.clone();
        let mut signalled = false;
        if process.cancel {
            processes
                .cancel(&process_id, scoped(controller, key, "cancel")?)
                .await
                .map_err(turn_handler_error)?;
        } else if process.waits_for_signal() {
            tokio::time::sleep(Duration::from_millis(u64::from(process.wake_delay_ms))).await;
            let event_type =
                lash_core::facade_support::process_signal_event_type("resume").map_err(terminal)?;
            let append =
                lash_core::ProcessEventAppendRequest::new(
                    event_type,
                    json!({ "key": key, "signal": "resume" }),
                )
                .with_replay_key(
                    lash_core::facade_support::process_signal_wait_key(&process_id, "resume", key),
                );
            processes
                .signal(
                    &process_id,
                    "resume",
                    key.clone(),
                    append,
                    scoped(controller, key, "signal")?,
                )
                .await
                .map_err(turn_handler_error)?;
            signalled = true;
        }
        let output = if process.await_result {
            serde_json::to_value(
                processes
                    .await_output(&process_id)
                    .await
                    .map_err(turn_handler_error)?,
            )
            .map_err(terminal)?
        } else {
            Value::Null
        };
        Ok(HostProcessReport {
            key: key.clone(),
            process_id: process_id.to_string(),
            created: receipt.disposition == lash_core::ProcessRegistrationOutcome::Created,
            signalled,
            cancel_requested: process.cancel,
            output,
        })
    }

    /// The cron owner's turn: its cell registers every schedule of the run.
    async fn cron_setup(
        &self,
        controller: &Controller<'_>,
        run: &str,
        session_id: String,
    ) -> HandlerResult<CronSetupReport> {
        let ctx = controller.context();
        let session = create_or_open_session(&self.core, session_id).await?;
        let outcome = session
            .send(TurnInput::text(format!(
                "Register the synthetic cron schedules. {CRON_SETUP_MARKER}{run} {WORKLOAD_MARKER}{}",
                self.load.sha256()
            )))
            .id(turn_id_for(&format!("{run}/cron")))
            .accept_restate(ctx)
            .await?
            .outcome_restate(ctx, RestateWait::new())
            .await?;
        Ok(CronSetupReport {
            worker_id: self.worker_id.clone(),
            outcome: input_outcome(outcome),
        })
    }

    /// One scheduled emission: publish it on the schedule's source and await
    /// every target process it started.
    async fn cron_tick(
        &self,
        controller: &Controller<'_>,
        generator: &Generator<'_>,
        subscription: u64,
        tick: u64,
    ) -> HandlerResult<CronTickReport> {
        let schedule = generator.cron_schedule(subscription);
        let key = generator.cron_tick_key(subscription, tick);
        let source = json!({ "schedule": schedule });
        let source_key =
            lash_core::facade_support::default_trigger_source_key(CRON_SOURCE_TYPE, &source);
        let report = self
            .core
            .triggers()
            .emit(
                lash::triggers::TriggerOccurrenceRequest::new(
                    CRON_SOURCE_TYPE,
                    source_key,
                    json!({ "schedule": schedule, "tick": tick.to_string() }),
                    key.clone(),
                )
                .with_source(source),
                scoped(controller, &key, "emit")?,
            )
            .await
            .map_err(turn_handler_error)?;
        let started = report.started_process_ids();
        let mut outputs = Vec::new();
        for process_id in &started {
            outputs.push(
                serde_json::to_value(
                    self.core
                        .processes()
                        .await_output(process_id)
                        .await
                        .map_err(turn_handler_error)?,
                )
                .map_err(terminal)?,
            );
        }
        Ok(CronTickReport {
            worker_id: self.worker_id.clone(),
            schedule,
            key,
            started_process_ids: started.iter().map(ToString::to_string).collect(),
            outputs,
        })
    }

    /// Delete a retired session, then prove a fresh open is refused.
    async fn delete_session(
        &self,
        ctx: WorkflowContext<'_>,
        session_id: String,
    ) -> HandlerResult<DeleteReport> {
        let administration = self
            .administration
            .get_or_init(|| async {
                lash_restate::RestateSessionAdministration::new(
                    self.core.session_administration().await,
                    self.restate_ingress_url.clone(),
                    self.restate_authority_id.clone(),
                )
            })
            .await;
        let execution = administration.for_invocation(ctx);
        execution
            .controller()
            .revoke_await_events_for_session(&SessionId::from(session_id.clone()))
            .await
            .map_err(terminal)?;
        let context = execution.delete_context(&session_id).map_err(terminal)?;
        let mut closing_waits = None;
        let deletion = match lash::LashCore::delete_session(context)
            .await
            .map_err(turn_handler_error)?
        {
            lash::SessionDeletion::Deleted(_) => DeletionOutcome::Deleted,
            lash::SessionDeletion::AlreadyDeleted { .. } => DeletionOutcome::AlreadyDeleted,
            lash::SessionDeletion::Closing(closing) => {
                closing_waits = Some(format!("{:?}", closing.waiting));
                DeletionOutcome::Closing
            }
        };
        // A closing session's physical delete is owed to the recovery relay:
        // wait for it, bounded, before proving a fresh open is refused.
        let started = Instant::now();
        let reopen_refusal = loop {
            match self.core.session(session_id.clone()).open().await {
                Err(error) => break Some(format!("{error:?}")),
                Ok(session) => {
                    session.close().await.map_err(turn_handler_error)?;
                    if deletion != DeletionOutcome::Closing
                        || started.elapsed() >= CLOSING_DELETE_WAIT
                    {
                        break None;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        };
        Ok(DeleteReport {
            worker_id: self.worker_id.clone(),
            session_id,
            deletion,
            reopen_refusal,
            refused_after_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            closing_waits,
        })
    }
}

fn scoped<'a>(
    controller: &'a Controller<'_>,
    key: &str,
    step: &str,
) -> HandlerResult<lash_core::ScopedEffectController<'a>> {
    controller
        .scoped_effect_controller(lash_core::AdmittedScope::runtime_operation(format!(
            "load:{key}:{step}"
        )))
        .map_err(terminal)
}

fn cancel_outcome(receipt: lash::CancelReceipt) -> CancelOutcome {
    match receipt {
        lash::CancelReceipt::Withdrawn(_) => CancelOutcome::Withdrawn,
        lash::CancelReceipt::Requested { .. } => CancelOutcome::Requested,
        lash::CancelReceipt::AlreadySettled { .. } => CancelOutcome::AlreadySettled,
        lash::CancelReceipt::NotFound => CancelOutcome::NotFound,
        _ => CancelOutcome::Unrecognized,
    }
}

fn input_outcome(outcome: lash::SendOutcome) -> InputOutcome {
    let (final_value, settled) = match &outcome.output {
        Some(output) => (
            output.result.final_value().cloned().unwrap_or(Value::Null),
            serde_json::to_value(&output.result.outcome)
                .unwrap_or_else(|error| json!({ "unencodable": error.to_string() })),
        ),
        None => (Value::Null, Value::Null),
    };
    InputOutcome {
        status: (&outcome.status).into(),
        root: outcome.root.as_ref().map(ToString::to_string),
        final_value,
        outcome: settled,
    }
}

/// Wait until the provider receipted a request for `operation`.
async fn wait_for_provider_receipt(witness: &PgPool, operation: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let asked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM witness_provider_receipts WHERE workflow_id = $1)",
        )
        .bind(operation)
        .fetch_one(witness)
        .await
        .with_context(|| format!("poll the provider receipt of `{operation}`"))?;
        if asked {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("the provider was never asked for `{operation}`")
}
