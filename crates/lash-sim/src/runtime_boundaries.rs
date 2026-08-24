use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::runtime::{
    QueuedWorkClaim, QueuedWorkClaimBoundary, RuntimeReplay, RuntimeScope, RuntimeSubject,
};
use lash_core::{
    ExecResponse, ExecutionScope, LeaseOwnerIdentity, PreparedToolCall, ProcessAwaitOutput,
    ProcessInput, ProcessProvenance, ProcessRegistration, ProcessRegistry, RecoveryContract,
    RuntimeCommit, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimePersistence, RuntimeSessionState, SessionExecutionLeaseClaimOutcome, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, ToolAttemptLaunch, ToolCallOutput,
    ToolCallRecord, ToolId,
};
use serde_json::{Value, json};

use crate::clock::SimClock;
use crate::scheduler::{BoundaryEvent, BoundaryKind};
use crate::trace::value_digest;

mod process_lifecycle;

use process_lifecycle::{
    LifecycleSuccessEngine, RecordingWorkerFaultSink, lifecycle_process_fact, lifecycle_worker,
    record_lifecycle_started, register_lifecycle_row, register_rerunnable_lifecycle_row,
};

const EFFECT_SCOPE_ID: &str = "lash-sim-runtime-boundaries";
const LEASE_TTL_MS: u64 = 30_000;

#[derive(Clone)]
pub enum RuntimeEffectReplayStore {
    Memory,
    SqliteFile(PathBuf),
    Postgres(Arc<lash_postgres_store::PostgresStorage>),
}

impl RuntimeEffectReplayStore {
    pub fn sqlite_file(path: impl Into<PathBuf>) -> Self {
        Self::SqliteFile(path.into())
    }

    pub fn postgres(storage: Arc<lash_postgres_store::PostgresStorage>) -> Self {
        Self::Postgres(storage)
    }

    fn controller_name(&self) -> &'static str {
        match self {
            Self::Memory | Self::SqliteFile(_) => "sqlite_runtime_effect_controller",
            Self::Postgres(_) => "postgres_runtime_effect_controller",
        }
    }
}

impl fmt::Debug for RuntimeEffectReplayStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory => f.write_str("Memory"),
            Self::SqliteFile(path) => f.debug_tuple("SqliteFile").field(path).finish(),
            Self::Postgres(_) => f.write_str("Postgres"),
        }
    }
}

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

#[derive(Clone, Debug)]
struct DurableEntry {
    result_digest: String,
    execution_count: usize,
    replay_count: usize,
}

pub struct RuntimeBoundaryHarness {
    store_factory: Arc<dyn SessionStoreFactory>,
    effect_replay_store: RuntimeEffectReplayStore,
    effect_controller: Option<Arc<dyn RuntimeEffectController>>,
    durable_entries: BTreeMap<String, DurableEntry>,
    delivered_process_wake_source_keys: BTreeSet<String>,
    worker_process_registry: Option<Arc<dyn ProcessRegistry>>,
    worker_process_continuations: Option<Arc<dyn lash_core::ProcessContinuationStore>>,
    clock: Arc<SimClock>,
}

impl RuntimeBoundaryHarness {
    pub(crate) fn new(
        store_factory: Arc<dyn SessionStoreFactory>,
        effect_replay_store: RuntimeEffectReplayStore,
        clock: Arc<SimClock>,
    ) -> Self {
        Self {
            store_factory,
            effect_replay_store,
            effect_controller: None,
            durable_entries: BTreeMap::new(),
            delivered_process_wake_source_keys: BTreeSet::new(),
            worker_process_registry: None,
            worker_process_continuations: None,
            clock,
        }
    }

    /// Read a real session-execution-lease fencing token from the store for a
    /// per-session probe scope. Each reading claims the probe lease and then
    /// releases it; because the in-memory store keeps the per-scope fencing
    /// counter across a release, the very next claim acquires a strictly higher
    /// token. The returned token is the ground truth the lease-time-monotonic
    /// oracle checks — unlike a generator-fed tick, it can actually regress if
    /// the lease store's fencing is broken. Deterministic: a single owner on a
    /// dedicated per-session scope, no wall-clock reads.
    pub async fn lease_probe_fencing_token(
        &mut self,
        session: &str,
    ) -> Result<u64, RuntimeBoundaryError> {
        let probe_scope = format!("{session}::lease-probe");
        let store = self.store_for_session(&probe_scope).await?;
        let owner = LeaseOwnerIdentity::opaque(
            "lash-sim-lease-probe",
            format!("{probe_scope}:probe-owner"),
        );
        let lease = match store
            .try_claim_session_execution_lease(
                &probe_scope,
                &owner,
                "lease-probe-fencing-token-executor",
                LEASE_TTL_MS,
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("claim lease probe failed: {err}")))?
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                return Err(RuntimeBoundaryError::new(format!(
                    "lease probe claim unexpectedly busy for `{probe_scope}`; holder fence={}",
                    holder.fencing_token
                )));
            }
        };
        let fencing_token = lease.fencing_token;
        // Release so the next probe re-acquires at a higher fencing token rather
        // than renewing the same one (a same-owner live claim would not advance).
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("release lease probe failed: {err}"))
            })?;
        Ok(fencing_token)
    }

    pub async fn deliver(&mut self, event: &BoundaryEvent) -> Result<Value, RuntimeBoundaryError> {
        match event.kind {
            BoundaryKind::Tool => self.complete_tool(event).await,
            BoundaryKind::ExecCode => self.execute_code(event).await,
            BoundaryKind::DurableEffect => self.complete_durable_effect(event).await,
            BoundaryKind::ProcessWake => self.deliver_process_wake(event).await,
            BoundaryKind::ProcessLifecycle => self.run_process_lifecycle(event).await,
            BoundaryKind::Worker => self.run_worker_stale_completion(event).await,
            kind => Err(RuntimeBoundaryError::new(format!(
                "runtime boundary harness does not own {}",
                kind.name()
            ))),
        }
    }

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
        let effect_id = event
            .payload
            .get("runtime_effect")
            .and_then(|value| value.get("effect_id"))
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::new(event.actor_alias.clone()),
                effect_id.clone(),
                RuntimeEffectKind::ToolAttempt,
                durable_key.clone(),
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
        let local_calls = Arc::new(AtomicUsize::new(0));
        let local_calls_for_executor = Arc::clone(&local_calls);
        let scripted_result = requested_result.clone();
        let call_id = effect_id.clone();
        let recorded_intents =
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: event.actor_alias.clone(),
                    request: lash_core::ProcessStartRequest::external(
                        format!("{effect_id}:intent-child"),
                        lash_core::ProcessOriginator::host_scoped("lash-sim-durable-effect"),
                        json!({"durable_key": durable_key}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Abandon,
                },
            ))]);
        let controller = self.ensure_effect_controller().await?;
        let outcome = controller
            .execute_effect(
                envelope.clone(),
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    local_calls_for_executor.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(ToolAttemptLaunch::Done {
                            record: Box::new(ToolCallRecord {
                                call_id: Some(call_id),
                                tool: "sim_opaque_effect".to_string(),
                                args: Value::Null,
                                output: ToolCallOutput::success(scripted_result),
                                duration_ms: 0,
                            }),
                            intents: recorded_intents,
                        }),
                        triggers: Vec::new(),
                    })
                }),
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("durable effect failed: {err}")))?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = &outcome else {
            return Err(RuntimeBoundaryError::new(
                "durable effect controller returned non-tool-attempt outcome",
            ));
        };
        let ToolAttemptLaunch::Done { record, .. } = &**launch else {
            return Err(RuntimeBoundaryError::new(
                "durable effect controller returned pending tool attempt",
            ));
        };
        let local_execution_count = local_calls.load(Ordering::SeqCst);
        let replayed = local_execution_count == 0;
        let result_digest = value_digest(&record.output.value_for_projection());
        let entry = match self.durable_entries.entry(durable_key.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => slot.insert(DurableEntry {
                result_digest: result_digest.clone(),
                execution_count: local_execution_count,
                replay_count: usize::from(replayed),
            }),
            std::collections::btree_map::Entry::Occupied(slot) => {
                let entry = slot.into_mut();
                if replayed {
                    entry.replay_count += 1;
                } else {
                    entry.execution_count += local_execution_count;
                }
                entry
            }
        };
        Ok(json!({
            "durable_key": durable_key,
            "result_digest": entry.result_digest,
            "execution_count": entry.execution_count,
            "replay_count": entry.replay_count,
            "replayed": replayed,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ToolAttempt.as_str(),
                "effect_id": effect_id,
                "replay_key": envelope.invocation.replay_key(),
                "envelope_hash": envelope_hash,
                "controller": self.effect_replay_store.controller_name(),
                "local_executor_called": local_execution_count > 0,
            },
            "runtime_effect_outcome": outcome,
        }))
    }

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
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::new(event.actor_alias.clone()),
                format!("tool-attempt:{}", event.boundary_id),
                RuntimeEffectKind::ToolAttempt,
                format!("tool/{}/{}", event.actor_alias, event.boundary_id),
            ),
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let local_calls = Arc::new(AtomicUsize::new(0));
        let local_calls_for_executor = Arc::clone(&local_calls);
        let output_for_executor = output.clone();
        let tool_name_for_executor = tool_name.clone();
        let call_id_for_executor = event.boundary_id.clone();
        let controller = self.ensure_effect_controller().await?;
        let outcome = controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    local_calls_for_executor.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(ToolAttemptLaunch::Done {
                            record: Box::new(ToolCallRecord {
                                call_id: Some(call_id_for_executor),
                                tool: tool_name_for_executor,
                                args,
                                output: ToolCallOutput::success(output_for_executor),
                                duration_ms: 0,
                            }),
                            intents: lash_core::ToolIntents::default(),
                        }),
                        triggers: Vec::new(),
                    })
                }),
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
        let execution_count = local_calls.load(Ordering::SeqCst);
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
                "controller": self.effect_replay_store.controller_name(),
                "local_executor_called": execution_count > 0,
            },
        }))
    }

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
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::new(event.actor_alias.clone()),
                format!("exec-code:{}", event.boundary_id),
                RuntimeEffectKind::ExecCode,
                format!("exec/{}/{}", event.actor_alias, event.boundary_id),
            ),
            RuntimeEffectCommand::ExecCode {
                language: "lash-sim-script".to_string(),
                code,
            },
        );
        let response = ExecResponse {
            observations: vec![output.clone()],
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: (exit_code != 0).then(|| format!("exit code {exit_code}")),
            duration_ms: 0,
            degraded_bindings: Vec::new(),
            terminal_finish: Some(json!({
                "output": output,
                "exit_code": exit_code,
            })),
        };
        let local_calls = Arc::new(AtomicUsize::new(0));
        let local_calls_for_executor = Arc::clone(&local_calls);
        let response_for_executor = response.clone();
        let controller = self.ensure_effect_controller().await?;
        let outcome = controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    local_calls_for_executor.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ExecCode {
                        result: Box::new(Ok(response_for_executor)),
                    })
                }),
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("exec-code effect failed: {err}")))?;
        let execution_count = local_calls.load(Ordering::SeqCst);
        Ok(json!({
            "session": event.actor_alias,
            "exec_output": output,
            "exit_code": exit_code,
            "execution_count": execution_count,
            "runtime_effect_outcome": outcome,
            "runtime_effect": {
                "kind": RuntimeEffectKind::ExecCode.as_str(),
                "controller": self.effect_replay_store.controller_name(),
                "local_executor_called": execution_count > 0,
            },
        }))
    }

    pub async fn deliver_process_wake(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let session = boundary_session_alias(event);
        let process_id = event
            .payload
            .get("process_id")
            .and_then(Value::as_str)
            .map_or_else(
                || format!("sim-process-{}", event.boundary_id.replace(':', "-")),
                ToString::to_string,
            );
        let sequence = event
            .payload
            .get("sequence")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let source_key = lash_core::facade_support::process_wake_source_key(&process_id, sequence);
        let replay_key = event
            .payload
            .get("replay_key")
            .and_then(Value::as_str)
            .unwrap_or(&source_key)
            .to_string();
        let wake = lash_core::facade_support::process_wake_delivery(
            lash_core::facade_support::ProcessWakeDeliveryRequest {
                target_session_id: session.clone(),
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
                event_invocation: RuntimeInvocation {
                    scope: RuntimeScope::new(session.clone()),
                    subject: RuntimeSubject::ProcessEvent {
                        process_id: process_id.clone(),
                        sequence,
                        event_type: "process.wake".to_string(),
                    },
                    caused_by: None,
                    replay: Some(RuntimeReplay {
                        key: replay_key,
                        attribution: None,
                    }),
                },
                process_caused_by: None,
                authority: lash_core::QueuedWorkAuthority::default(),
                wake: lash_core::facade_support::ProcessWake {
                    input: format!("wake for {session}"),
                },
                occurred_at: std::time::UNIX_EPOCH + std::time::Duration::from_millis(event.at),
            },
        )
        .map_err(|err| RuntimeBoundaryError::new(format!("process wake failed: {err}")))?;
        let store = self.store_for_session(&session).await?;
        let owner = LeaseOwnerIdentity::opaque(
            "lash-sim-process-wake-driver",
            format!("{}:process-wake-driver", session),
        );
        let lease = match store
            .try_claim_session_execution_lease(
                &session,
                &owner,
                "deliver-process-wake-executor",
                LEASE_TTL_MS,
            )
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("claim session lease failed: {err}"))
            })? {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                return Ok(json!({
                    "session": session,
                    "process_wake": true,
                    "process_id": process_id,
                    "sequence": wake.sequence,
                    "wake_id": wake.wake_id,
                    "claimed_once": false,
                    "lease_busy": true,
                    "busy_holder": owner_json(&holder.owner),
                    "runtime_process_wake": wake,
                    "runtime_queued_work": {
                        "source_key": source_key,
                        "work_class": "ProcessWake",
                        "enqueued": false,
                        "claimed": false,
                        "claim_id_present": false,
                    },
                }));
            }
        };
        // A wake redelivered with the same structural process/event source key
        // must be claimed exactly once. This driver models the durable
        // receiver-evidence dedupe contract, and it
        // settles the claimed wake below so it never lingers as reclaimable
        // queued work that a subsequent runtime turn would double-claim.
        let duplicate = self
            .delivered_process_wake_source_keys
            .contains(&source_key);
        let batch = store
            .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(wake.clone()))
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("enqueue wake failed: {err}")))?;
        let claim = if duplicate {
            None
        } else {
            store
                .claim_ready_queued_work_by_batch_ids(
                    &session,
                    &lease.fence(),
                    &owner,
                    QueuedWorkClaimBoundary::Idle,
                    std::slice::from_ref(&batch.batch_id),
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await
                .map_err(|err| {
                    RuntimeBoundaryError::new(format!("claim queued wake failed: {err}"))
                })?
                .claim
        };
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!(
                    "release process wake session lease failed: {err}"
                ))
            })?;
        let claimed_once = claim.is_some();
        if let Some(claim) = &claim {
            // Consume the delivered wake: hand the claim back and remove the
            // now-unheld batch so it is not reclaimable after this delivery. This
            // mirrors the real runtime settling a claimed wake in its turn, and
            // keeps a redelivery from surfacing the same work to another claimant.
            store
                .abandon_queued_work_claim(claim)
                .await
                .map_err(|err| {
                    RuntimeBoundaryError::new(format!("abandon delivered wake claim failed: {err}"))
                })?;
        }
        // Remove the batch this delivery enqueued (whether it was claimed here or
        // was a redelivery of an already-consumed wake) so no lingering queued
        // work leaks to a later claimant.
        let settled = store
            .cancel_queued_work_batch(&session, &batch.batch_id)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("settle delivered wake batch failed: {err}"))
            })?
            .ok_or_else(|| {
                RuntimeBoundaryError::new(format!(
                    "settle delivered wake batch returned no removal for `{}`",
                    batch.batch_id
                ))
            })?;
        if settled.batch_id != batch.batch_id {
            return Err(RuntimeBoundaryError::new(format!(
                "settle delivered wake removed `{}` instead of `{}`",
                settled.batch_id, batch.batch_id
            )));
        }
        if claimed_once {
            self.delivered_process_wake_source_keys.insert(source_key);
        }
        Ok(json!({
            "session": session,
            "process_wake": true,
            "process_id": process_id,
            "sequence": wake.sequence,
            "wake_id": wake.wake_id,
            "claimed_once": claimed_once,
            "runtime_process_wake": wake,
            "runtime_queued_work": {
                "source_key": batch.source_key,
                "work_class": batch.work_class().map(|class| format!("{class:?}")),
                "enqueued": true,
                "claimed": claimed_once,
                "claimed_batch_count": claim.as_ref().map_or(0, |claim| claim.batches.len()),
                "claim_fencing_token": claim.as_ref().map(|claim| claim.fencing_token),
                "batch_id_present": !batch.batch_id.is_empty(),
                "claim_id_present": claim.as_ref().is_some_and(|claim| !claim.claim_id.is_empty()),
                "runtime_turn_id": claim.as_ref().map(|_| format!("wake-turn:{}", event.boundary_id)),
            },
        }))
    }

    pub async fn run_worker_stale_completion(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let session = boundary_session_alias(event);
        let store = self.store_for_session(&session).await?;
        let stale_owner = LeaseOwnerIdentity::opaque(
            event.actor_alias.clone(),
            format!("{}:incarnation-001", event.actor_alias),
        );
        let live_owner = LeaseOwnerIdentity::opaque(
            event.actor_alias.clone(),
            format!("{}:incarnation-002", event.actor_alias),
        );
        // Worker one (the doomed incarnation) acquires the session execution lease
        // and starts a real unit of worker-owned queued work.
        let stale_lease = match store
            .try_claim_session_execution_lease(
                &session,
                &stale_owner,
                "run-worker-stale-completion-executor",
                LEASE_TTL_MS,
            )
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("claim stale lease failed: {err}")))?
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { .. } => {
                return Ok(json!({
                    "worker_alias": event.actor_alias,
                    "session": session,
                    "initial_owner": owner_json(&stale_owner),
                    "active_owner": owner_json(&live_owner),
                    "stale_completion_rejected": false,
                    "lease_busy": true,
                    "runtime_worker_store": {
                        "session_execution_lease_acquired_after_ttl": false,
                        "stale_completion_left_live_lease_renewable": false,
                        "busy_during_in_flight_turn": true,
                    },
                }));
            }
        };
        let work = self
            .start_worker_owned_work(
                store.as_ref(),
                &session,
                &stale_owner,
                &stale_lease,
                event.at,
            )
            .await?;

        // Worker one crashes mid-flight. The scheduled world advances beyond
        // the real TTL before worker two takes over; no host wall clock or
        // process-liveness shortcut establishes expiry.
        self.clock.advance_by(LEASE_TTL_MS + 1).await;
        let live_lease = match store
            .try_claim_session_execution_lease(
                &session,
                &live_owner,
                "run-worker-stale-completion-executor-2",
                LEASE_TTL_MS,
            )
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("take over expired worker lease failed: {err}"))
            })? {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition.lease,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                return Err(RuntimeBoundaryError::new(format!(
                    "worker lease reclaim unexpectedly busy for `{session}`; holder={}",
                    holder.owner.owner_id
                )));
            }
        };

        let process_completion = self
            .run_process_completion_contention(
                &session,
                &event.boundary_id,
                &stale_owner,
                &live_owner,
            )
            .await?;

        // Worker two resumes the crashed worker's in-flight work under its own
        // lease and rejects the dead owner's stale completion attempt.
        let failover = self
            .resume_crashed_worker_work(store.as_ref(), &session, &live_owner, &live_lease, &work)
            .await?;
        let expired_owner_commit_rejected = failover.stale_work_completion_rejected;

        let stale_completion = stale_lease.completion();
        let stale_release_error = match store
            .release_session_execution_lease(&stale_completion)
            .await
        {
            Err(error) => error,
            Ok(()) => {
                return Err(RuntimeBoundaryError::new(
                    "stale worker completion was silently accepted",
                ));
            }
        };
        if !matches!(
            stale_release_error,
            StoreError::SessionExecutionLeaseReleaseRefused { .. }
        ) {
            return Err(RuntimeBoundaryError::new(format!(
                "stale worker completion returned the wrong refusal: {stale_release_error}"
            )));
        }
        let renewed_live = store
            .renew_session_execution_lease(&live_lease.fence(), LEASE_TTL_MS)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!(
                    "live worker lease was cleared by stale completion: {err}"
                ))
            })?;
        store
            .release_session_execution_lease(&renewed_live.completion())
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("release live lease failed: {err}"))
            })?;
        Ok(json!({
            "worker_alias": event.actor_alias,
            "session": session,
            "initial_owner": owner_json(&stale_owner),
            "active_owner": owner_json(&live_owner),
            "active_fencing_token": renewed_live.fencing_token,
            "stale_completion_rejected": true,
            "stale_completion_refusal": "SessionExecutionLeaseReleaseRefused",
            "expired_owner_commit_rejected": expired_owner_commit_rejected,
            "process_stale_completion_rejected": process_completion.stale_rejected,
            "process_stale_output_absent": process_completion.stale_output_absent,
            "process_terminal_writer": process_completion.terminal_writer.clone(),
            "process_terminal_event_count": process_completion.terminal_event_count,
            "lease_owner_changed": !stale_owner.same_incarnation(&live_owner),
            "runtime_stale_completion": {
                "session_id": stale_completion.session_id,
                "owner": owner_json(&stale_completion.owner),
                "fencing_token": stale_completion.fencing_token,
                "lease_token_present": !stale_completion.lease_token.is_empty(),
            },
            "runtime_active_lease": {
                "session_id": renewed_live.session_id,
                "owner": owner_json(&renewed_live.owner),
                "fencing_token": renewed_live.fencing_token,
                "lease_token_present": !renewed_live.lease_token.is_empty(),
            },
            "runtime_worker_store": {
                "session_execution_lease_acquired_after_ttl": true,
                "takeover_after_ttl_expiry": true,
                "stale_completion_left_live_lease_renewable": true,
                "process_completion": {
                    "process_id": process_completion.process_id,
                    "stale_fencing_token": process_completion.stale_fencing_token,
                    "live_fencing_token": process_completion.live_fencing_token,
                    "fencing_token_advanced": process_completion.live_fencing_token
                        > process_completion.stale_fencing_token,
                    "stale_completion_rejected": process_completion.stale_rejected,
                    "stale_output_absent": process_completion.stale_output_absent,
                    "terminal_writer": process_completion.terminal_writer,
                    "terminal_event_count": process_completion.terminal_event_count,
                },
                "worker_owned_work": {
                    "batch_id_present": !work.batch_id.is_empty(),
                    "source_key": work.source_key,
                    "first_owner_claim_fencing_token": work.claim_fencing_token,
                    "first_owner_claimed_work": true,
                    "second_owner_resumed_work": failover.resumed_by_second_owner,
                    "second_owner_claim_fencing_token": failover.resumed_claim_fencing_token,
                    "second_owner_outranks_first": failover.resumed_claim_fencing_token
                        > work.claim_fencing_token,
                    "stale_work_completion_rejected": failover.stale_work_completion_rejected,
                },
            },
        }))
    }

    /// Drive a self-contained ADR 0019 recovery scenario against a REAL
    /// `DurableProcessWorker` over a fresh in-memory process registry, and record
    /// the disposition-driven verdicts. One boundary delivery exercises all four
    /// process-lifecycle operations end to end:
    ///
    /// - (a) **spawn-with-disposition**: register a started OwnerBound row, a
    ///   Rerunnable sibling, and an OwnerBound row carrying an Abandon Request.
    /// - (b) **worker-crash**: the OwnerBound/Rerunnable rows' starter stops
    ///   renewing; its unexpired lease keeps the OwnerBound row non-terminal.
    /// - (c) **drive-sweep**: a fresh worker runs the disposition-driven recovery.
    /// - (d) **abandon-request**: the operator-authorized OwnerBound row reconciles
    ///   once its lease has lapsed.
    ///
    /// The recorded facts (terminal, writer, evidence, independently-observed
    /// lease/authorization) are the ground truth the `process_never_double_started`
    /// and `abandoned_requires_evidence` oracles verify. The registry is in-memory
    /// (independent of the session-store backend), so the recorded observation is
    /// identical across the cross-backend replay lanes.
    pub async fn run_process_lifecycle(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, RuntimeBoundaryError> {
        let session = boundary_session_alias(event);
        let registry: Arc<dyn lash_core::ProcessRegistry> =
            Arc::new(lash_core::TestLocalProcessRegistry::default());

        // A sweep claimant and a crashed holder with distinct incarnations.
        let sweep_owner =
            LeaseOwnerIdentity::opaque("sim-recovery", format!("recovery-claimant:{session}"));
        let dead_holder =
            LeaseOwnerIdentity::opaque("sim-dead-owner", format!("before-the-crash:{session}"));
        let silent_owner =
            LeaseOwnerIdentity::opaque("sim-silent-owner", format!("sim-silent-owner:{session}"));
        let mut runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        runtime_host.process_engines = lash_core::facade_support::ProcessEngineRegistry::new()
            .with_engine(Arc::new(LifecycleSuccessEngine));
        let policy = lash_core::SessionPolicy {
            provider_id: "sim-lifecycle".to_string(),
            model: lash_core::ModelSpec::builder("sim-lifecycle-model")
                .context_window_tokens(16_384)
                .build()
                .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?,
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let env_ref = lash_core::runtime::persist_process_execution_env(
            runtime_host.durability.process_env_store.as_ref(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                policy.clone(),
            ),
        )
        .await
        .map_err(|err| RuntimeBoundaryError::new(err.to_string()))?;

        // (a)+(b): a started OwnerBound row whose holder crashed while still
        // holding a live lease, and a Rerunnable sibling left mid-flight.
        register_lifecycle_row(
            registry.as_ref(),
            "ob-crashed",
            RecoveryContract::OwnerBound,
        )
        .await?;
        record_lifecycle_started(registry.as_ref(), "ob-crashed", &dead_holder).await?;
        match registry
            .claim_process_lease("ob-crashed", &dead_holder, LEASE_TTL_MS)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("dead holder lease claim failed: {err}"))
            })? {
            lash_core::ProcessLeaseClaimOutcome::Acquired(_) => {}
            lash_core::ProcessLeaseClaimOutcome::Busy { .. } => {
                return Err(RuntimeBoundaryError::new(
                    "dead holder's lease claim was unexpectedly busy",
                ));
            }
        }
        register_rerunnable_lifecycle_row(registry.as_ref(), "rerun-crashed", env_ref).await?;
        record_lifecycle_started(registry.as_ref(), "rerun-crashed", &dead_holder).await?;

        // (d): a started OwnerBound row whose silent holder's lease has lapsed
        // (none held) and for which an operator recorded an Abandon Request.
        register_lifecycle_row(
            registry.as_ref(),
            "ob-abandon-req",
            RecoveryContract::OwnerBound,
        )
        .await?;
        record_lifecycle_started(registry.as_ref(), "ob-abandon-req", &silent_owner).await?;
        registry
            .request_process_abandon(
                "ob-abandon-req",
                lash_core::AbandonRequest {
                    requested_by: "sim-operator".to_string(),
                    requested_at_ms: event.at,
                    reason: Some("host retired".to_string()),
                },
            )
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("record abandon request failed: {err}"))
            })?;

        // (c): the disposition-driven recovery sweep.
        let fault_sink = RecordingWorkerFaultSink::default();
        let worker = lifecycle_worker(
            Arc::clone(&registry),
            sweep_owner.clone(),
            runtime_host,
            policy,
            &fault_sink,
        );
        let _ = worker.drive_pending_processes().await.map_err(|err| {
            RuntimeBoundaryError::new(format!("recovery sweep dispatch failed: {err}"))
        })?;
        let awaiter = lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry));

        let ob_crashed = lifecycle_process_fact(
            &registry,
            &awaiter,
            "ob-crashed",
            RecoveryContract::OwnerBound,
            Some(&dead_holder),
            &sweep_owner,
        )
        .await?;
        let rerun_crashed = lifecycle_process_fact(
            &registry,
            &awaiter,
            "rerun-crashed",
            RecoveryContract::Rerunnable,
            Some(&dead_holder),
            &sweep_owner,
        )
        .await?;
        let ob_abandon_req = lifecycle_process_fact(
            &registry,
            &awaiter,
            "ob-abandon-req",
            RecoveryContract::OwnerBound,
            None,
            &sweep_owner,
        )
        .await?;

        // The sweep admits and returns; a fault after that return is the only
        // thing that could leave these verdicts describing work that never
        // happened. Recording zero of them is part of the observation.
        let sweep_faults = fault_sink.recorded();
        if !sweep_faults.is_empty() {
            return Err(RuntimeBoundaryError::new(format!(
                "recovery sweep reported worker faults: {sweep_faults:?}"
            )));
        }

        Ok(json!({
            "session": session,
            "process_lifecycle": true,
            "runtime_process_lifecycle": {
                "sweep_driven": true,
                "sweep_worker_faults": sweep_faults.len(),
                "processes": [ob_crashed, rerun_crashed, ob_abandon_req],
            },
        }))
    }

    /// Worker one claims a real unit of queued work under its session execution
    /// lease, modelling a worker-owned turn that is in flight when the worker
    /// crashes.
    async fn start_worker_owned_work(
        &self,
        store: &dyn RuntimePersistence,
        session: &str,
        owner: &LeaseOwnerIdentity,
        lease: &lash_core::SessionExecutionLease,
        occurred_at_ms: u64,
    ) -> Result<WorkerOwnedWork, RuntimeBoundaryError> {
        let wake = worker_failover_work(session, occurred_at_ms)?;
        let source_key =
            lash_core::facade_support::process_wake_source_key(&wake.process_id, wake.sequence);
        let batch = store
            .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(wake))
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("enqueue worker-owned work failed: {err}"))
            })?;
        let claim = store
            .claim_ready_queued_work_by_batch_ids(
                session,
                &lease.fence(),
                owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&batch.batch_id),
                lash_core::testing::queued_work_claim_policy(1),
            )
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("first worker claim of work failed: {err}"))
            })?
            .ok_or_else(|| {
                RuntimeBoundaryError::new(
                    "first worker could not claim its own queued work".to_string(),
                )
            })?;
        Ok(WorkerOwnedWork {
            batch_id: batch.batch_id,
            source_key,
            claim_fencing_token: claim.fencing_token,
            claim,
        })
    }

    /// Worker two reclaims the crashed worker's in-flight work, resumes it, and
    /// proves the dead owner's stale work completion is rejected.
    async fn resume_crashed_worker_work(
        &self,
        store: &dyn RuntimePersistence,
        session: &str,
        owner: &LeaseOwnerIdentity,
        lease: &lash_core::SessionExecutionLease,
        work: &WorkerOwnedWork,
    ) -> Result<WorkerFailover, RuntimeBoundaryError> {
        // The crashed worker's claim remains attached to the row here. The
        // successor must reclaim through the generation-mismatch predicate;
        // clearing the claim first would reduce this proof to the unclaimed-row
        // path and mask a broken generation cutover.
        let resumed = store
            .claim_ready_queued_work_by_batch_ids(
                session,
                &lease.fence(),
                owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&work.batch_id),
                lash_core::testing::queued_work_claim_policy(1),
            )
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("second worker claim of work failed: {err}"))
            })?
            .ok_or_else(|| {
                RuntimeBoundaryError::new(
                    "second worker could not resume the crashed worker's queued work".to_string(),
                )
            })?;
        // The dead owner's late completion of its now-superseded claim must be
        // rejected, never silently accepted. The reclaim under the live lease
        // rewrote the batch's claim id + lease token, so settling the crashed
        // worker's original claim through the runtime commit path is rejected as
        // superseded (ADR 0029).
        let current = store.load_session().await.map_err(|err| {
            RuntimeBoundaryError::new(format!(
                "load current head before stale claim completion: {err}"
            ))
        })?;
        let (head_revision, session_graph, persisted_node_ids) = current.map_or_else(
            || (0, lash_core::SessionGraph::default(), HashSet::new()),
            |read| {
                let persisted_node_ids = read
                    .graph
                    .nodes
                    .iter()
                    .map(|node| node.node_id.clone())
                    .collect();
                (read.head_revision, read.graph, persisted_node_ids)
            },
        );
        let stale_state = RuntimeSessionState {
            session_id: session.to_string(),
            session_graph,
            persisted_node_ids,
            head_revision,
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        let stale_work_completion_rejected = matches!(
            store
                .commit_runtime_state(
                    RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                        .completing_queue_claim(work.claim.completion()),
                )
                .await,
            Err(lash_core::StoreError::QueuedWorkClaimSuperseded { .. })
        );
        if !stale_work_completion_rejected {
            return Err(RuntimeBoundaryError::new(
                "crashed worker's stale work completion was not rejected after failover"
                    .to_string(),
            ));
        }
        let resumed_claim_fencing_token = resumed.fencing_token;
        // Settle the resumed work so it does not linger as reclaimable queued
        // work once this boundary releases the worker's session lease. Under
        // generation fencing a released-but-uncompleted claim is reclaimable, so
        // an unsettled batch would otherwise be double-claimed by a later runtime
        // turn. Hand the claim back and remove the now-unheld batch.
        store
            .abandon_queued_work_claim(&resumed)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("abandon resumed worker claim failed: {err}"))
            })?;
        store
            .cancel_queued_work_batch(session, &work.batch_id)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!("settle resumed worker batch failed: {err}"))
            })?;
        Ok(WorkerFailover {
            resumed_by_second_owner: true,
            resumed_claim_fencing_token,
            stale_work_completion_rejected,
        })
    }

    /// Exercise the process-registry terminal fence itself. Session and queued
    /// work leases are useful surrounding evidence, but they cannot prove that a
    /// stale process owner is unable to persist its semantic terminal output.
    async fn run_process_completion_contention(
        &mut self,
        session: &str,
        boundary_id: &str,
        stale_owner: &LeaseOwnerIdentity,
        live_owner: &LeaseOwnerIdentity,
    ) -> Result<WorkerProcessCompletion, RuntimeBoundaryError> {
        let registry = self.ensure_worker_process_registry().await?;
        // A generated workload may contain several worker-contention boundaries
        // for one session. The scheduler boundary id is stable across backend
        // replays and unique per occurrence, so it keeps each proof independent
        // without introducing a timing- or delivery-order-derived counter.
        let process_id = format!("sim-worker-process-{session}-{boundary_id}");
        registry
            .register_process(ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: json!({"simulation": "worker_stale_completion"}),
                },
                RecoveryContract::Rerunnable,
                ProcessProvenance::host(),
            ))
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("register process: {err}")))?;

        // TTL zero deterministically makes A reclaimable without sleeping or
        // depending on the simulator host's wall clock.
        let stale_lease = registry
            .claim_process_lease(&process_id, stale_owner, 0)
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("claim process A: {err}")))?
            .acquired()
            .ok_or_else(|| RuntimeBoundaryError::new("process A lease was busy"))?;
        let live_lease = registry
            .claim_process_lease(&process_id, live_owner, LEASE_TTL_MS)
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("take over process as B: {err}")))?
            .acquired()
            .ok_or_else(|| RuntimeBoundaryError::new("process B takeover was busy"))?;

        let stale_output = ProcessAwaitOutput::Success {
            value: json!({"writer": "stale", "must_not_persist": true}),
            control: None,
        };
        let stale_rejected = registry
            .complete_process_with_lease(&stale_lease, stale_output)
            .await
            .is_err();
        if !stale_rejected {
            return Err(RuntimeBoundaryError::new(
                "stale process owner persisted terminal output".to_string(),
            ));
        }
        let stale_output_absent = terminal_writer(registry.as_ref(), &process_id)
            .await?
            .is_none();
        if !stale_output_absent {
            return Err(RuntimeBoundaryError::new(
                "stale process terminal output remained in the event log".to_string(),
            ));
        }

        let successor_output = ProcessAwaitOutput::Success {
            value: json!({"writer": "successor", "completed": true}),
            control: None,
        };
        registry
            .complete_process_with_lease(&live_lease, successor_output.clone())
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("complete process as B: {err}")))?;
        // A same-output redelivery after the atomic lease release must replay,
        // not append another terminal event.
        registry
            .complete_process_with_lease(&live_lease, successor_output)
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("replay process B output: {err}")))?;
        let events = registry
            .events_after(&process_id, 0)
            .await
            .map_err(|err| RuntimeBoundaryError::new(format!("read process events: {err}")))?;
        let terminal_event_count = events
            .iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count();
        let terminal_writer = terminal_writer_from_events(&events);
        if terminal_writer.as_deref() != Some("successor") || terminal_event_count != 1 {
            return Err(RuntimeBoundaryError::new(format!(
                "successor process terminal was not unique: writer={terminal_writer:?}, count={terminal_event_count}"
            )));
        }
        Ok(WorkerProcessCompletion {
            process_id,
            stale_fencing_token: stale_lease.fencing_token,
            live_fencing_token: live_lease.fencing_token,
            stale_rejected,
            stale_output_absent,
            terminal_writer: terminal_writer.unwrap_or_default(),
            terminal_event_count,
        })
    }

    async fn ensure_worker_process_registry(
        &mut self,
    ) -> Result<Arc<dyn ProcessRegistry>, RuntimeBoundaryError> {
        if let Some(registry) = self.worker_process_registry.as_ref() {
            return Ok(Arc::clone(registry));
        }
        let (registry, continuations): (
            Arc<dyn ProcessRegistry>,
            Arc<dyn lash_core::ProcessContinuationStore>,
        ) = match &self.effect_replay_store {
            RuntimeEffectReplayStore::Memory => {
                let store = Arc::new(lash_core::TestLocalProcessRegistry::default());
                (store.clone(), store)
            }
            RuntimeEffectReplayStore::SqliteFile(path) => {
                let process_path = path.with_extension("process-registry.sqlite");
                let store = Arc::new(
                    lash_sqlite_store::SqliteProcessRegistry::open(
                        &process_path,
                        process_path.with_extension("sessions"),
                    )
                    .await
                    .map_err(|err| {
                        RuntimeBoundaryError::new(format!(
                            "open SQLite process registry `{}`: {err}",
                            process_path.display()
                        ))
                    })?,
                );
                (store.clone(), store)
            }
            RuntimeEffectReplayStore::Postgres(storage) => {
                let store = Arc::new(storage.process_registry());
                (store.clone(), store)
            }
        };
        self.worker_process_registry = Some(Arc::clone(&registry));
        self.worker_process_continuations = Some(continuations);
        Ok(registry)
    }

    #[cfg(test)]
    async fn ensure_worker_process_continuations(
        &mut self,
    ) -> Result<Arc<dyn lash_core::ProcessContinuationStore>, RuntimeBoundaryError> {
        self.ensure_worker_process_registry().await?;
        self.worker_process_continuations
            .as_ref()
            .cloned()
            .ok_or_else(|| RuntimeBoundaryError::new("process continuation store unavailable"))
    }

    async fn ensure_effect_controller(
        &mut self,
    ) -> Result<Arc<dyn RuntimeEffectController>, RuntimeBoundaryError> {
        if let Some(controller) = &self.effect_controller {
            return Ok(controller.clone());
        }
        let scope = ExecutionScope::runtime_operation(EFFECT_SCOPE_ID);
        let controller: Arc<dyn RuntimeEffectController> = match &self.effect_replay_store {
            RuntimeEffectReplayStore::Memory => Arc::new(
                lash_sqlite_store::SqliteRuntimeEffectController::memory_with_clock(
                    scope,
                    self.clock.clone(),
                )
                .await
                .map_err(|err| {
                    RuntimeBoundaryError::new(format!(
                        "open in-memory effect replay controller failed: {err}"
                    ))
                })?,
            ),
            RuntimeEffectReplayStore::SqliteFile(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        RuntimeBoundaryError::new(format!(
                            "create effect replay directory `{}` failed: {err}",
                            parent.display()
                        ))
                    })?;
                }
                Arc::new(
                    lash_sqlite_store::SqliteRuntimeEffectController::open_with_clock(
                        path,
                        scope,
                        self.clock.clone(),
                    )
                    .await
                    .map_err(|err| {
                        RuntimeBoundaryError::new(format!(
                            "open sqlite effect replay controller `{}` failed: {err}",
                            path.display()
                        ))
                    })?,
                )
            }
            RuntimeEffectReplayStore::Postgres(storage) => {
                Arc::new(storage.runtime_effect_controller(scope))
            }
        };
        self.effect_controller = Some(Arc::clone(&controller));
        Ok(controller)
    }

    async fn store_for_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<dyn RuntimePersistence>, RuntimeBoundaryError> {
        let request = SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        if let Some(store) = self
            .store_factory
            .open_existing_store(&request)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!(
                    "open existing runtime store for `{session_id}` failed: {err}"
                ))
            })?
        {
            return Ok(store);
        }
        self.store_factory
            .create_store(&request)
            .await
            .map_err(|err| {
                RuntimeBoundaryError::new(format!(
                    "create runtime store for `{session_id}` failed: {err}"
                ))
            })
    }
}

struct WorkerOwnedWork {
    batch_id: String,
    source_key: String,
    claim_fencing_token: u64,
    claim: QueuedWorkClaim,
}

struct WorkerFailover {
    resumed_by_second_owner: bool,
    resumed_claim_fencing_token: u64,
    stale_work_completion_rejected: bool,
}

struct WorkerProcessCompletion {
    process_id: String,
    stale_fencing_token: u64,
    live_fencing_token: u64,
    stale_rejected: bool,
    stale_output_absent: bool,
    terminal_writer: String,
    terminal_event_count: usize,
}

async fn terminal_writer(
    registry: &dyn ProcessRegistry,
    process_id: &str,
) -> Result<Option<String>, RuntimeBoundaryError> {
    let events = registry
        .events_after(process_id, 0)
        .await
        .map_err(|err| RuntimeBoundaryError::new(format!("read process events: {err}")))?;
    Ok(terminal_writer_from_events(&events))
}

fn terminal_writer_from_events(events: &[lash_core::ProcessEvent]) -> Option<String> {
    events.iter().find_map(|event| {
        let terminal = event.semantics.terminal.as_ref()?;
        let ProcessAwaitOutput::Success { value, .. } = &terminal.outcome else {
            return None;
        };
        value
            .get("writer")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn worker_failover_work(
    session: &str,
    occurred_at_ms: u64,
) -> Result<lash_core::ProcessWakeDelivery, RuntimeBoundaryError> {
    let process_id = format!("sim-worker-{session}");
    lash_core::facade_support::process_wake_delivery(
        lash_core::facade_support::ProcessWakeDeliveryRequest {
            target_session_id: session.to_string(),
            process_id: process_id.clone(),
            sequence: 1,
            event_type: "process.wake".to_string(),
            event_invocation: RuntimeInvocation {
                scope: RuntimeScope::new(session.to_string()),
                subject: RuntimeSubject::ProcessEvent {
                    process_id,
                    sequence: 1,
                    event_type: "process.wake".to_string(),
                },
                caused_by: None,
                replay: Some(RuntimeReplay {
                    attribution: None,
                    key: format!("worker-failover:{session}:work"),
                }),
            },
            process_caused_by: None,
            authority: lash_core::QueuedWorkAuthority::default(),
            wake: lash_core::facade_support::ProcessWake {
                input: format!("worker-owned work for {session}"),
            },
            occurred_at: std::time::UNIX_EPOCH + std::time::Duration::from_millis(occurred_at_ms),
        },
    )
    .map_err(|err| RuntimeBoundaryError::new(format!("build worker-owned work failed: {err}")))
}

fn boundary_session_alias(event: &BoundaryEvent) -> String {
    event
        .payload
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(&event.actor_alias)
        .to_string()
}

fn owner_json(owner: &LeaseOwnerIdentity) -> Value {
    json!({
        "owner_id": owner.owner_id,
        "incarnation_id": owner.incarnation_id,
    })
}

#[cfg(test)]
mod tests;
