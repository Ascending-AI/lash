//! The worker half of the load workload: one Restate workflow whose handler
//! runs one [`LoadRequest`] through lash's public API.

use super::{
    ActiveOperations, CRON_SETUP_MARKER, CRON_SOURCE_TYPE, CancelOutcome, CronSetupReport,
    CronTickReport, DeleteReport, DeletionOutcome, HostProcessReport, InputOutcome, LoadContext,
    LoadRequest, LoadResponse, QUEUED_MARKER, QueuedReport, TURN_MARKER, TurnReport,
    WORKLOAD_MARKER, turn_id_for,
};
use crate::{journaled_session, turn_handler_error};
use anyhow::Result;
use lash::restate::RestateRuntimeEffectController;
use lash::restate::RestateWait;
use lash::restate::restate_sdk;
use lash::runtime::AwaitEventResolver as _;
use lash::{SessionId, TurnInput};
use lash_perf::workload::{Generator, ProcessPlan, QueuedInputPlan, TurnPlan};
use restate_sdk::context::{ContextSideEffects, RunFuture};
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
    use lash::restate::restate_sdk;
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

#[async_trait::async_trait]
trait WorkloadProcessCleanup: Sync {
    fn timestamp_ms(&self) -> u64 {
        u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
    }

    async fn owned(&self, session: &str) -> Result<Vec<lash::ProcessId>>;
    async fn cancel(&self, process: &lash::ProcessId) -> Result<()>;
    async fn await_terminal(&self, process: &lash::ProcessId) -> Result<()>;
}

/// The deadline is recorded once. Every clock check and timed read runs
/// inside a recorded step: replay reads its outcome, and a retried step
/// retains the original deadline.
async fn cleanup_model_children(
    ctx: &WorkflowContext<'_>,
    admin: &impl WorkloadProcessCleanup,
    session: &str,
) -> HandlerResult<usize> {
    let deadline = ctx
        .run(|| async { Ok(admin.timestamp_ms().saturating_add(20_000)) })
        .name("load.model-children.deadline")
        .await?;
    let mut cleaned = std::collections::BTreeSet::new();
    loop {
        let children = ctx
            .run(|| async {
                cleanup_read_before_deadline(admin, deadline, admin.owned(session))
                    .await
                    .map(Json)
            })
            .name("load.model-children.list")
            .await?
            .0;
        if children.is_empty() {
            return Ok(cleaned.len());
        }
        for child in &children {
            admin.cancel(child).await.map_err(terminal_chain)?;
        }
        for child in children {
            ctx.run(|| async {
                cleanup_read_before_deadline(admin, deadline, admin.await_terminal(&child)).await
            })
            .name("load.model-children.terminal")
            .await?;
            cleaned.insert(child);
        }
    }
}

/// This body is only called inside ctx.run, including its clock reads and
/// timeout result. A replay never re-evaluates either decision.
async fn cleanup_read_before_deadline<T>(
    admin: &impl WorkloadProcessCleanup,
    deadline: u64,
    read: impl std::future::Future<Output = Result<T>>,
) -> HandlerResult<T> {
    let expired = || {
        terminal(
            "workload model-child cleanup did not reach terminal state: journaled deadline elapsed",
        )
    };
    let remaining = deadline
        .checked_sub(admin.timestamp_ms())
        .filter(|remaining| *remaining > 0)
        .ok_or_else(expired)?;
    let answer = tokio::time::timeout(Duration::from_millis(remaining), read)
        .await
        .map_err(|_| expired())?
        .map_err(terminal_chain)?;
    if admin.timestamp_ms() >= deadline {
        return Err(expired());
    }
    Ok(answer)
}

struct SessionProcessCleanup<'a, 'ctx> {
    processes: lash::process::Processes,
    controller: &'a Controller<'ctx>,
}

#[async_trait::async_trait]
impl WorkloadProcessCleanup for SessionProcessCleanup<'_, '_> {
    async fn owned(&self, session: &str) -> Result<Vec<lash::ProcessId>> {
        Ok(self
            .processes
            .list_originated_by(
                &lash::process::SessionScope::new(SessionId::parse(session)?),
                &lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::any_of([
                        lash::process::ProcessStatus::Running,
                        lash::process::ProcessStatus::Waiting,
                    ]),
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .map(|row| row.process_id)
            .collect())
    }

    async fn cancel(&self, process: &lash::ProcessId) -> Result<()> {
        let scope = scoped(self.controller, process.as_ref(), "model-child-cleanup")
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        self.processes.cancel(process, scope).await?;
        Ok(())
    }

    async fn await_terminal(&self, process: &lash::ProcessId) -> Result<()> {
        self.processes.await_output(process).await?;
        Ok(())
    }
}

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
    restate_authority_id: lash::restate::RestateAuthorityId,
    model: lash::LlmProfileConfig,
    active: ActiveOperations,
    administration: Arc<tokio::sync::OnceCell<lash::restate::RestateSessionAdministration>>,
}

pub struct LoadWorkerConfig {
    pub worker_id: String,
    pub core: lash::LashCore,
    pub witness: PgPool,
    pub load: LoadContext,
    pub restate_ingress_url: String,
    pub restate_authority_id: lash::restate::RestateAuthorityId,
    pub model: lash::LlmProfileConfig,
    /// Where each handler counts itself while it runs.
    pub active: ActiveOperations,
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
            active: config.active,
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
        let _running = self.active.enter(request.workflow_key());
        let response = match request {
            LoadRequest::Behaviors {
                workload_sha256,
                run,
            } => {
                self.load
                    .require_workload(&workload_sha256)
                    .map_err(terminal_chain)?;
                let controller = RestateRuntimeEffectController::new(
                    ctx,
                    self.restate_authority_id.clone(),
                    self.core.build_generation().clone(),
                );
                LoadResponse::Behaviors(Box::new(
                    Box::pin(self.behaviors(&controller, &run)).await?,
                ))
            }
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
                let controller = RestateRuntimeEffectController::new(
                    ctx,
                    self.restate_authority_id.clone(),
                    self.core.build_generation().clone(),
                );
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
                let controller = RestateRuntimeEffectController::new(
                    ctx,
                    self.restate_authority_id.clone(),
                    self.core.build_generation().clone(),
                );
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
                let controller = RestateRuntimeEffectController::new(
                    ctx,
                    self.restate_authority_id.clone(),
                    self.core.build_generation().clone(),
                );
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
    /// cancel what the plan cancels, await every run, then run its host
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
        let session = journaled_session(
            ctx,
            &self.core,
            lash::SessionId::parse(session_id.clone()).map_err(terminal)?,
        )
        .await?;
        let input = generator
            .record(id.actor, id.ordinal, "input", plan.input_bytes)
            .map_err(terminal_chain)?;
        // A run states no prompt (FIG-4589): the plan's context bytes ride
        // after the turn's input.
        let context = generator.text(id.actor, id.ordinal, "prompt", plan.prompt_bytes);
        let main = session
            .send(TurnInput::text(format!(
                "Run the synthetic load turn. {TURN_MARKER}{operation} {WORKLOAD_MARKER}{}\n{input}\n## Synthetic load context\n\n{context}",
                self.load.sha256()
            )))
            .id(lash::TurnId::parse(turn_id_for(&operation)).map_err(terminal)?)
            .accept_restate(ctx)
            .await?;
        // A queued input or a cancel is meant for a running run: wait until
        // the provider has been asked for this turn's cell.
        let needs_active_run = plan.cancel
            || plan
                .queued_inputs
                .iter()
                .any(|queued| queued.during_active_turn);
        if needs_active_run {
            super::provider_watch::wait_for_provider_receipt(
                &self.core,
                &self.witness,
                main.receipt(),
                &operation,
            )
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
        // Inputs that arrive once the session is idle run as their own runs.
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
        session: &lash::DurableSession,
        input: &QueuedInputPlan,
    ) -> HandlerResult<lash::SendHandle> {
        session
            .send(TurnInput::text(format!(
                "Run the synthetic queued input. {QUEUED_MARKER}{} {WORKLOAD_MARKER}{}",
                input.idempotency_key,
                self.load.sha256()
            )))
            .id(lash::TurnId::parse(turn_id_for(&input.idempotency_key)).map_err(terminal)?)
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
                lash::rlm::lang::Declaration::Process(process) => Some(process),
                _ => None,
            })
            .ok_or_else(|| terminal(format!("the body of `{key}` declares no process")))?;
        let process_name = declaration.name.to_string();
        // A host pin keeps the module and environment alive for the process (ADR 0113).
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
        let environment = lash::process::ProcessExecutionEnvSpec::new(
            lash::plugins::AdmittedPluginConfig::default(),
            lash::runtime::SessionPolicy {
                model: Some(self.model.clone()),
                ..lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                )
            },
        );
        let env_ref = self
            .core
            .host_artifacts()
            .publish_process_env(&pin, &environment)
            .await
            .map_err(turn_handler_error)?;
        let request = lash::process::ProcessStartRequest::new(
            input,
            lash::process::ProcessOriginator::host(),
            lash::process::Lifetime::Detached,
        )
        .with_host_start_key(key.as_bytes())
        .with_env_ref(env_ref)
        .with_extra_event_types(
            lash::process::lashlang_process_event_types()
                .into_iter()
                .chain(
                    lash::process::lashlang_process_signal_event_types(declaration).map_err(
                        |source| {
                            turn_handler_error(
                                lash::plugins::PluginError::UnusableSchema {
                                    source: Box::new(source),
                                }
                                .into_turn_failure(
                                    lash::runtime::RuntimeErrorCode::PluginSessionManager,
                                )
                                .into(),
                            )
                        },
                    )?,
                ),
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
            let signal = lash::process::ProcessSignal::new(
                lash::process::ProcessSignalIdentity::new(
                    process_id.clone(),
                    "resume",
                    key.clone(),
                )
                .map_err(terminal)?,
                json!({ "key": key, "signal": "resume" }),
            );
            processes
                .signal(signal, scoped(controller, key, "signal")?)
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
            created: receipt.disposition == lash::process::ProcessRegistrationOutcome::Created,
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
        let session = journaled_session(
            ctx,
            &self.core,
            lash::SessionId::parse(session_id).map_err(terminal)?,
        )
        .await?;
        let outcome = session
            .send(TurnInput::text(format!(
                "Register the synthetic cron schedules. {CRON_SETUP_MARKER}{run} {WORKLOAD_MARKER}{}",
                self.load.sha256()
            )))
            .id(lash::TurnId::parse(turn_id_for(&format!("{run}/cron"))).map_err(terminal)?)
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
        let source_key = lash::triggers::default_trigger_source_key(CRON_SOURCE_TYPE, &source);
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
                lash::restate::RestateSessionAdministration::new(
                    self.core.session_administration().await,
                    self.restate_ingress_url.clone(),
                    self.restate_authority_id.clone(),
                    self.core.build_generation().clone(),
                )
            })
            .await;
        let execution = administration.for_invocation(ctx);
        let cleaned = cleanup_model_children(
            execution.controller().context(),
            &SessionProcessCleanup {
                processes: self.core.processes(),
                controller: execution.controller(),
            },
            &session_id,
        )
        .await?;
        println!("load cleanup session={session_id} model_children_cancelled={cleaned}");
        execution
            .controller()
            .revoke_await_events_for_session(
                &SessionId::parse(session_id.clone()).map_err(terminal)?,
            )
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
            lash::SessionDeletion::Absent { .. } => DeletionOutcome::Absent,
            lash::SessionDeletion::Closing(closing) => {
                closing_waits = Some(format!("{:?}", closing.waiting));
                DeletionOutcome::Closing
            }
        };
        // A closing session's physical delete is owed to the recovery relay:
        // wait for it, bounded, before proving a fresh open is refused.
        let started = Instant::now();
        let reopen_refusal = loop {
            match self
                .core
                .session(lash::SessionId::parse(session_id.clone()).map_err(terminal)?)
                .open()
                .await
            {
                Err(error) => break Some(format!("{error:?}")),
                Ok(session) => {
                    session
                        .close()
                        .await
                        .map_err(|refused| turn_handler_error(refused.into()))?;
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
) -> HandlerResult<lash::runtime::ScopedEffectController<'a>> {
    controller
        .scoped_effect_controller(lash::runtime::AdmittedScope::runtime_operation(format!(
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
    let (final_value, settled) = match outcome.output() {
        Some(output) => (
            output.result.final_value().cloned().unwrap_or(Value::Null),
            serde_json::to_value(&output.result.outcome)
                .unwrap_or_else(|error| json!({ "unencodable": error.to_string() })),
        ),
        None => (Value::Null, Value::Null),
    };
    InputOutcome {
        status: (&outcome.status()).into(),
        run: outcome.run().map(ToString::to_string),
        final_value,
        outcome: settled,
    }
}

#[path = "behaviors.rs"]
mod behaviors;
#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
