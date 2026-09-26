//! The runtime boundaries the generated world delivers beside its turns: a
//! tool attempt, an exec-code run and a durable effect. Each runs inside a
//! handler on the world's engine, where a deployment runs an effect, through
//! the handler's own scoped controller.

use lash_sansio::SessionId;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::runtime::RuntimeAttribution;
use lash_core::sync::MutexExt as _;
use lash_core::{
    AdmittedScope, EffectAddress, ExecResponse, ExecutionScope, PreparedToolCall,
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ToolAttemptLaunch, ToolCallOutput, ToolCallRecord, ToolId,
};
use serde_json::{Value, json};

use crate::scheduler::{BoundaryEvent, BoundaryKind};
use crate::trace::value_digest;

pub(crate) const EFFECT_SCOPE_ID: &str = "lash-sim-runtime-boundaries";

/// The scope a tool or exec-code boundary's effect runs under: a turn of the
/// boundary's session, as a session's own turn runs its effects.
fn boundary_effect_scope(event: &BoundaryEvent) -> ExecutionScope {
    ExecutionScope::turn(
        event.actor_alias.clone(),
        format!("{EFFECT_SCOPE_ID}:{}", event.boundary_id),
    )
}

/// The scope a durable effect runs under: a turn of the effect's session,
/// one per durable key, so its crash and redrive replay one invocation.
pub(crate) fn durable_effect_scope(session: &str, durable_key: &str) -> ExecutionScope {
    ExecutionScope::turn(
        session.to_string(),
        format!("{EFFECT_SCOPE_ID}:{durable_key}"),
    )
}

/// The controller every boundary effect runs on, as its observation names it.
const RUNTIME_EFFECT_CONTROLLER: &str = "restate_runtime_effect_controller";

#[derive(Debug)]
pub struct RuntimeBoundaryError {
    message: String,
}

impl RuntimeBoundaryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeBoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeBoundaryError {}

pub struct RuntimeBoundaryHarness {
    engine: crate::backend::SimEngine,
}

/// One effect a boundary runs in a handler: its envelope and the outcome its
/// local executor produces when the engine runs it.
struct BoundaryEffect {
    envelope: RuntimeEffectEnvelope,
    outcome: RuntimeEffectOutcome,
}

/// What one handler execution of a [`BoundaryEffect`] observed.
#[derive(Default)]
struct EffectRun {
    outcome: Option<Result<RuntimeEffectOutcome, String>>,
    local_calls: usize,
}

impl RuntimeBoundaryHarness {
    pub(crate) fn new(engine: crate::backend::SimEngine) -> Self {
        Self { engine }
    }

    pub async fn deliver(&mut self, event: &BoundaryEvent) -> Result<Value, RuntimeBoundaryError> {
        match event.kind {
            BoundaryKind::Tool => self.complete_tool(event).await,
            BoundaryKind::ExecCode => self.execute_code(event).await,
            BoundaryKind::DurableEffect => self.complete_durable_effect(event).await,
            kind => Err(RuntimeBoundaryError::new(format!(
                "runtime boundary harness does not own {kind}"
            ))),
        }
    }

    /// The admitted runtime-operation scope a boundary's effect runs under.
    fn admitted(scope: &ExecutionScope) -> AdmittedScope {
        AdmittedScope::new(scope.clone())
    }

    /// One handler attempt that runs `effect` on the handler's controller and
    /// records what it saw into `run`. A crashing attempt dies right after the
    /// engine recorded the effect, as a deployment dying mid-handler does.
    fn attempt(
        effect: &Arc<BoundaryEffect>,
        run: &Arc<Mutex<EffectRun>>,
        crash_after_effect: bool,
    ) -> lash_restate_test::HandlerAttempt {
        let effect = Arc::clone(effect);
        let run = Arc::clone(run);
        Arc::new(move |scoped| {
            let effect = Arc::clone(&effect);
            let run = Arc::clone(&run);
            Box::pin(async move {
                let calls = Arc::new(AtomicUsize::new(0));
                let executor_calls = Arc::clone(&calls);
                let scripted = effect.outcome.clone();
                let outcome = scoped
                    .execute_effect(
                        effect.envelope.clone(),
                        RuntimeEffectLocalExecutor::testing(move |_| async move {
                            executor_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(scripted)
                        }),
                    )
                    .await
                    .map_err(|err| err.to_string());
                {
                    let mut recorded = run.lock_recover();
                    recorded.local_calls += calls.load(Ordering::SeqCst);
                    recorded.outcome = Some(outcome);
                }
                if crash_after_effect {
                    std::panic::resume_unwind(Box::new("the attempt dies after its effect"));
                }
            })
        })
    }

    /// Run `effect` once inside a handler under `scope`.
    async fn run_once(
        &self,
        scope: &ExecutionScope,
        effect: BoundaryEffect,
    ) -> Result<(RuntimeEffectOutcome, usize), RuntimeBoundaryError> {
        let effect = Arc::new(effect);
        let run = Arc::new(Mutex::new(EffectRun::default()));
        self.engine
            .restate()
            .run_in_handler(Self::admitted(scope), Self::attempt(&effect, &run, false))
            .await
            .map_err(RuntimeBoundaryError::new)?;
        let mut run = run.lock_recover();
        let outcome = run
            .outcome
            .take()
            .ok_or_else(|| RuntimeBoundaryError::new("the effect's handler recorded no outcome"))?
            .map_err(RuntimeBoundaryError::new)?;
        Ok((outcome, run.local_calls))
    }

    /// A durable effect under crash and redrive: the first attempt runs the
    /// effect and dies after the engine recorded it; the engine replays the
    /// invocation into a redrive whose executor would produce
    /// `redrive_outcome`. The recorded effect must not run again: the redrive
    /// is served the first attempt's outcome.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn complete_durable_effect(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let durable_key = event
            .payload
            .get("durable_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let requested_result = event
            .payload
            .get("result")
            .cloned()
            .unwrap_or_else(|| json!({"completed": true}));
        let redrive_result = event
            .payload
            .get("redrive_result")
            .cloned()
            .unwrap_or_else(|| json!({"completed": false}));
        let effect_id = event
            .payload
            .get("runtime_effect")
            .and_then(|value| value.get("effect_id"))
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let scope = durable_effect_scope(&event.actor_alias, &durable_key);
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                EffectAddress::new(scope.clone(), durable_key.clone())
                    .expect("durable effect carries an admitted effect scope"),
                RuntimeAttribution::for_session(event.actor_alias.clone()),
                effect_id.clone(),
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: PreparedToolCall::from_parts(
                    effect_id.clone(),
                    ToolId::from("tool:sim_opaque_effect"),
                    "sim_opaque_effect",
                    json!({
                        "durable_key": durable_key,
                        "session": event.actor_alias,
                    }),
                    None,
                    json!({"prepared_by": "lash-sim"}),
                ),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let envelope_hash = envelope.stable_hash().map_err(|err| {
            RuntimeBoundaryError::new(format!("durable effect envelope hash failed: {err}"))
        })?;
        let recorded_intents =
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: SessionId::from(event.actor_alias.clone()),
                    declaration: lash_core::ProcessStartDeclaration::external(
                        lash_core::ProcessOriginator::host_scoped("lash-sim-durable-effect"),
                        json!({"durable_key": durable_key}),
                        lash_core::ProcessLifecyclePolicy::new(
                            lash_core::ParentScope::Host,
                            lash_core::OnParentEnd::Abandon,
                        ),
                    ),
                },
            ))]);
        let tool_outcome = |result: Value| RuntimeEffectOutcome::ToolAttempt {
            launch: Box::new(ToolAttemptLaunch::Done {
                record: Box::new(ToolCallRecord {
                    call_id: Some(effect_id.clone()),
                    tool: "sim_opaque_effect".to_string(),
                    args: Value::Null,
                    output: ToolCallOutput::success(result),
                }),
                intents: recorded_intents.clone(),
            }),
            triggers: Vec::new(),
            capture: None,
        };
        let first = Arc::new(BoundaryEffect {
            envelope: envelope.clone(),
            outcome: tool_outcome(requested_result),
        });
        let redrive = Arc::new(BoundaryEffect {
            envelope: envelope.clone(),
            outcome: tool_outcome(redrive_result),
        });
        let first_run = Arc::new(Mutex::new(EffectRun::default()));
        let redrive_run = Arc::new(Mutex::new(EffectRun::default()));
        self.engine
            .restate()
            .run_crashed_then_redriven(
                Self::admitted(&scope),
                Self::attempt(&first, &first_run, true),
                Self::attempt(&redrive, &redrive_run, false),
            )
            .await
            .map_err(RuntimeBoundaryError::new)?;
        let (first_outcome, first_calls) = {
            let mut run = first_run.lock_recover();
            let outcome = run
                .outcome
                .take()
                .ok_or_else(|| {
                    RuntimeBoundaryError::new("the crashing attempt recorded no outcome")
                })?
                .map_err(RuntimeBoundaryError::new)?;
            (outcome, run.local_calls)
        };
        let (redrive_outcome, redrive_calls) = {
            let mut run = redrive_run.lock_recover();
            let outcome = run
                .outcome
                .take()
                .ok_or_else(|| RuntimeBoundaryError::new("the redrive recorded no outcome"))?
                .map_err(RuntimeBoundaryError::new)?;
            (outcome, run.local_calls)
        };
        let digest = |outcome: &RuntimeEffectOutcome| -> Result<String, RuntimeBoundaryError> {
            let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
                return Err(RuntimeBoundaryError::new(
                    "durable effect returned a non-tool-attempt outcome",
                ));
            };
            let ToolAttemptLaunch::Done { record, .. } = &**launch else {
                return Err(RuntimeBoundaryError::new(
                    "durable effect returned a pending tool attempt",
                ));
            };
            Ok(value_digest(&record.output.value_for_projection()))
        };
        let first_digest = digest(&first_outcome)?;
        let redrive_digest = digest(&redrive_outcome)?;
        let replayed = redrive_calls == 0;
        Ok(json!({
            "durable_key": durable_key,
            "result_digest": first_digest,
            "redrive_result_digest": redrive_digest,
            "redrive_served_recorded_result": first_digest == redrive_digest,
            "execution_count": first_calls + redrive_calls,
            "replay_count": usize::from(replayed),
            "replayed": replayed,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ToolAttempt.as_str(),
                "effect_id": effect_id,
                "replay_key": envelope.invocation.replay_key(),
                "envelope_hash": envelope_hash,
                "controller": RUNTIME_EFFECT_CONTROLLER,
                "local_executor_called": first_calls > 0,
                "redrive_local_executor_called": redrive_calls > 0,
            },
            "runtime_effect_outcome": redrive_outcome,
        }))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn complete_tool(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let tool_name = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("sim_tool")
            .to_string();
        let output = event
            .payload
            .get("output")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let args = json!({
            "boundary_id": event.boundary_id,
            "session": event.actor_alias,
        });
        let call = PreparedToolCall::from_parts(
            event.boundary_id.clone(),
            ToolId::from(format!("tool:{tool_name}")),
            tool_name.clone(),
            args.clone(),
            None,
            json!({"prepared_by": "lash-sim"}),
        );
        let scope = boundary_effect_scope(event);
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                EffectAddress::new(
                    scope.clone(),
                    format!("tool/{}/{}", event.actor_alias, event.boundary_id),
                )
                .expect("tool boundary carries an admitted effect scope"),
                RuntimeAttribution::for_session(event.actor_alias.clone()),
                format!("tool-attempt:{}", event.boundary_id),
            ),
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let (outcome, execution_count) = self
            .run_once(
                &scope,
                BoundaryEffect {
                    envelope,
                    outcome: RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(ToolAttemptLaunch::Done {
                            record: Box::new(ToolCallRecord {
                                call_id: Some(event.boundary_id.clone()),
                                tool: tool_name.clone(),
                                args,
                                output: ToolCallOutput::success(output.clone()),
                            }),
                            intents: lash_core::ToolIntents::default(),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    },
                },
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("tool effect failed: {err}")))?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
            return Err(RuntimeBoundaryError::new(
                "tool controller returned non-tool-attempt outcome",
            ));
        };
        let ToolAttemptLaunch::Done { record, .. } = *launch else {
            return Err(RuntimeBoundaryError::new(
                "sim tool boundary unexpectedly returned pending tool launch",
            ));
        };
        Ok(json!({
            "session": event.actor_alias,
            "tool_output": output,
            "tool_name": tool_name,
            "tool_call_id": event.boundary_id,
            "execution_count": execution_count,
            "runtime_tool_output": record.output,
            "runtime_tool_record": record,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ToolAttempt.as_str(),
                "controller": RUNTIME_EFFECT_CONTROLLER,
                "local_executor_called": execution_count > 0,
            },
        }))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub async fn execute_code(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let output = event
            .payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let exit_code = event
            .payload
            .get("exit_code")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let code = format!("sim_exec('{}')", event.boundary_id);
        let scope = boundary_effect_scope(event);
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                EffectAddress::new(
                    scope.clone(),
                    format!("exec/{}/{}", event.actor_alias, event.boundary_id),
                )
                .expect("exec boundary carries an admitted effect scope"),
                RuntimeAttribution::for_session(event.actor_alias.clone()),
                format!("exec-code:{}", event.boundary_id),
            ),
            RuntimeEffectCommand::ExecCode {
                language: "lash-sim-script".to_string(),
                code,
            },
        );
        let response = ExecResponse {
            observations: vec![lash_core::Observation {
                text: output.clone(),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: (exit_code != 0).then(|| {
                lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Program,
                    format!("exit code {exit_code}"),
                )
            }),
            degraded_bindings: Vec::new(),
            terminal_finish: Some(json!({
                "output": output,
                "exit_code": exit_code,
            })),
        };
        let (outcome, execution_count) = self
            .run_once(
                &scope,
                BoundaryEffect {
                    envelope,
                    outcome: RuntimeEffectOutcome::ExecCode {
                        result: Box::new(Ok(response)),
                    },
                },
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("exec-code effect failed: {err}")))?;
        Ok(json!({
            "session": event.actor_alias,
            "exec_output": output,
            "exit_code": exit_code,
            "execution_count": execution_count,
            "runtime_effect_outcome": outcome,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ExecCode.as_str(),
                "controller": RUNTIME_EFFECT_CONTROLLER,
                "local_executor_called": execution_count > 0,
            },
        }))
    }
}

#[cfg(test)]
mod tests;
