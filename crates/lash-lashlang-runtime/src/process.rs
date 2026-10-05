/// version_surface = "coexist"
/// version_guard(items(LASH_LASHLANG_PROGRAM_DOMAIN_VERSION, lashlang_program_hash))
const LASH_LASHLANG_PROGRAM_DOMAIN_VERSION: &str = "lash-lashlang-program/v3";

mod execution_result;
mod segment_state;
use execution_result::{
    process_lashlang_execution_result, process_lashlang_failure, process_worker_failure,
};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
pub use segment_state::LASHLANG_SEGMENT_STATE_VERSION;
use segment_state::{
    LashlangSegmentState, LashlangSegmentStateError, MAX_SEGMENT_CONTINUATION_BYTES,
    ReplayOrdinals, capture_segment, decode_lashlang_segment_state,
};
mod definition_holds;
use definition_holds::hold_segment_definitions;
use lash_sansio::{ProcessId, SessionId};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceBranchSelection, TraceEvent, TraceLanguageChildExecution, TraceLanguageExecution,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceNodeAwaited, TraceNodeWaitResolution, TraceRuntimeScope, TraceRuntimeSubject,
};
use lashlang::{ExecutionHost, ExecutionHostError};

use crate::{
    LASHLANG_ENGINE_KIND, LashlangHostEnvironmentCheck, LashlangHostError, LashlangProcessEngine,
    LashlangProcessFailureCode, LashlangProcessInput, ProcessHostOp,
    bridge::{
        lashlang_value_to_json, process_event_payload, process_sleep,
        protocol_tool_reply_to_lashlang_value,
    },
    resolve_lashlang_module_operation, validate_lashlang_process_admission,
};

static SEGMENT_BOUNDARY_DECLINED_TOTAL: AtomicU64 = AtomicU64::new(0);
#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

pub(crate) fn record_segment_boundary_decline(
    error: &dyn std::fmt::Display,
    message: &'static str,
) {
    let declined_total = SEGMENT_BOUNDARY_DECLINED_TOTAL
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    tracing::warn!(error = %error, declined_total, "{message}");
}

/// The executable generation a Lashlang process runs as (FIG-3571).
///
/// Its preimage is the process body's [`lashlang::ExecutableIdentity`] (the
/// module, the exported process, and the semantic-hash, bytecode,
/// instruction-accounting and node-id contracts the body compiles and reports
/// under) plus what this tier adds on top: the segment-state generation a
/// parked body resumes from, the replay-key grammar its journal is keyed by,
/// the host requirements it was admitted against. Nothing
/// stored says "this was written by version N": a run compares the generation
/// its start record names against the one this build mints for the same
/// payload, which is a pure function computed before the artifact is loaded.
///
/// Public because a readability preflight has no other way to ask the
/// question.
#[expect(
    clippy::expect_used,
    reason = "the identity is a tuple of strings and integer constants serialized straight to in-memory bytes"
)]
pub fn lashlang_program_hash(input: &LashlangProcessInput) -> String {
    let executable = lashlang::ExecutableIdentity::of(
        &input.module_ref,
        lashlang::Entry::Process(&input.process_ref),
    );
    let identity = serde_json::to_vec(&(
        executable.as_str(),
        LASHLANG_SEGMENT_STATE_VERSION,
        crate::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION,
        &input.host_requirements_ref,
    ))
    .expect("lashlang program identity should serialize");
    format!(
        "blake3:{}",
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_LASHLANG_PROGRAM_DOMAIN_VERSION,
            identity,
        )
    )
}

/// The identity fence: a segment parked by another program identity — another
/// bytecode generation, or another module — is refused with the shared resume
/// refusal naming the identity it recorded.
fn refuse_foreign_program(
    persisted: &str,
    current: &str,
    owner: Option<lash_core::LeaseOwnerIdentity>,
) -> Option<lash_core::ProcessAwaitOutput> {
    (persisted != current).then(|| {
        tracing::warn!(
            persisted,
            current,
            bytecode = lashlang::BYTECODE_FORMAT_VERSION,
            "lashlang segment was parked by another program identity; refusing to resume"
        );
        retired_generation(persisted.to_string(), owner)
    })
}

/// The shared resume refusal for a run whose stored generation this build
/// retired: the process ends Abandoned with `ResumeRefused {
/// RetiredGeneration }` naming the identity it found, before any effect.
fn retired_generation(
    found: String,
    owner: Option<lash_core::LeaseOwnerIdentity>,
) -> lash_core::ProcessAwaitOutput {
    lash_core::ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::ResumeRefused {
                reason: lash_core::ProcessResumeRefusal::RetiredGeneration { found },
            },
            owner,
            epoch_ms: lash_core::facade_support::current_epoch_ms(),
        }),
        control: None,
    }
}

pub(crate) fn validate_lashlang_process_for_run(
    artifact: &lash_vm_client::InspectedArtifact,
    input: &LashlangProcessInput,
    host: LashlangHostEnvironmentCheck<'_>,
) -> Result<(), Box<lash_core::ProcessAwaitOutput>> {
    validate_lashlang_process_admission(artifact, input, host).map_err(|refusal| {
        Box::new(process_lashlang_failure(
            refusal.failure_code(),
            refusal.to_string(),
            None,
        ))
    })
}

pub async fn run_lashlang_process(
    mut engine: LashlangProcessEngine,
    mut context: lash_core::ProcessEngineRunContext<'_>,
    payload: serde_json::Value,
) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
    let handover = context.take_handover();
    let ledger = WorkerRecoveryLedger::carried(handover.as_ref());
    // The reservation is live accounting, read outside any recorded step,
    // and its answer differs between executions of one segment. A refusal
    // therefore decides nothing the journal holds: the attempt fails
    // retryably with no command issued, where a terminal proposed here
    // would stand at a position a re-execution's journal already recorded
    // the body's commands at (ADR 0105 §1, FIG-4422). A body whose budget
    // stays exhausted parks once its engine's bounded retry runs out.
    let recovery = engine
        .workers
        .begin_execution_from(&ledger.scope(context.process_id()), ledger.totals)
        .await
        .map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(
                error.to_string(),
            ))
        })?;
    engine.workers = recovery.service().clone();
    let result = Box::pin(run_lashlang_process_scoped(
        engine, context, payload, handover, ledger,
    ))
    .await;
    recovery.settle().await.map_err(|error| {
        lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(error.to_string()))
    })?;
    result
}

#[expect(
    clippy::expect_used,
    reason = "admission accepts the host environment and the process substrate supplies attempt-bound write authority"
)]
async fn run_lashlang_process_scoped(
    engine: LashlangProcessEngine,
    context: lash_core::ProcessEngineRunContext<'_>,
    payload: serde_json::Value,
    handover: Option<lash_core::SegmentHandover>,
    worker_recovery: WorkerRecoveryLedger,
) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
    let is_initial_segment = handover.is_none();
    let segment_controller = context.scoped_effect_controller();
    let phase_probe = context.turn_phase_probe();
    let segment = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(|authority| authority.segment());
    let mut input = match LashlangProcessInput::from_payload(payload) {
        Ok(input) => input,
        Err(err) => {
            return Ok(process_lashlang_failure(
                LashlangProcessFailureCode::ProcessPayloadInvalid,
                format!("invalid lashlang process payload: {err}"),
                None,
            )
            .into());
        }
    };
    // The handover's integrity, before anything else of the run: the worker
    // already parked an incarnation its start stamped under another
    // executable generation (FIG-3571), so a handover naming another program
    // identity or segment-state generation than the stamp is a handover this
    // incarnation never wrote. It ends the process with the shared resume
    // refusal, naming what it found, exactly as a retired module artifact
    // does below (FIG-3588).
    let resume_owner = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(|authority| authority.invocation_started().map(|started| started.owner));
    let current_program_hash = lashlang_program_hash(&input);
    if let Some(refusal) = handover.as_ref().and_then(|handover| {
        refuse_foreign_program(
            &handover.program_hash,
            &current_program_hash,
            resume_owner.clone(),
        )
    }) {
        return Ok(refusal.into());
    }
    let mut segment_state: Option<LashlangSegmentState> = match handover {
        Some(handover) => match decode_lashlang_segment_state(&handover.engine_state) {
            Ok(state) => {
                if let Some(run) = &state.tool_run {
                    let refused = (|| {
                        run.check_capture(lash_core::tool_run::CutPhase::Capturable)?;
                        let successor = segment.ok_or(
                            lash_core::tool_run::ContinuationRefusal::MissingSegmentAuthority,
                        )?;
                        run.clone().adopt(
                            &lash_core::EffectOpener::process(context.process_id().clone()),
                            run.ledger()?.lifecycle(),
                            successor,
                        )?;
                        if run.environment != context.registration().env_ref {
                            return Err(
                                lash_core::tool_run::ContinuationRefusal::ForeignEnvironment,
                            );
                        }
                        Ok(())
                    })();
                    if let Err(refusal) = refused {
                        return Err(segment_state::continuation_refused(refusal));
                    }
                }
                // The parent's only look at the VM bytes: size, owner, the
                // VM contract they were written under, format and hash.
                let owner = segment_continuation_owner(context.process_id());
                let reads = lashlang::vm_contract_reads();
                if let Err(refusal) = state
                    .vm
                    .check(&segment_continuation_expectation(&owner, &reads))
                {
                    return Ok(process_lashlang_failure(
                        LashlangProcessFailureCode::ProcessSegmentHandoverInvalid,
                        format!("invalid lashlang segment handover: {refusal}"),
                        None,
                    )
                    .into());
                }
                Some(state)
            }
            Err(LashlangSegmentStateError::VersionMismatch { found, .. }) => {
                return Ok(retired_generation(
                    format!("lashlang-segment-state-v{found}"),
                    resume_owner,
                )
                .into());
            }
            Err(err @ LashlangSegmentStateError::FormatMismatch { .. }) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessSegmentHandoverInvalid,
                    format!("invalid lashlang segment handover: {err}"),
                    None,
                )
                .into());
            }
        },
        None => None,
    };
    let artifact = {
        let _phase = context.named_phase("rlm_process.load_artifact");
        match engine
            .workers
            .inspect_artifact(&engine.artifact_store, &input.module_ref)
            .await
        {
            Ok(Some(artifact)) => artifact,
            Ok(None) | Err(lash_core::ArtifactStoreError::ArtifactMissing { .. }) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessModuleArtifactMissing,
                    format!("missing lashlang module artifact `{}`", input.module_ref),
                    None,
                )
                .into());
            }
            Err(lash_core::ArtifactStoreError::UnsupportedGeneration { refusal }) => {
                tracing::warn!(module_ref = %input.module_ref, error = %refusal,
                    "refusing an unsupported module artifact generation");
                return Ok(retired_generation(input.module_ref.to_string(), resume_owner).into());
            }
            Err(lash_core::ArtifactStoreError::StoredDataCorrupt { source, .. }) => {
                return Ok(lash_core::ProcessAwaitOutput::Abandoned {
                    evidence: Box::new(lash_core::AbandonEvidence {
                        writer: lash_core::AbandonWriter::ResumeRefused {
                            reason: lash_core::ProcessResumeRefusal::StoredArtifactCorrupt {
                                artifact_ref: input.module_ref.to_string(),
                                source,
                            },
                        },
                        owner: resume_owner,
                        epoch_ms: lash_core::facade_support::current_epoch_ms(),
                    }),
                    control: None,
                }
                .into());
            }
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err.into()));
            }
        }
    };
    input.process_name = artifact
        .process_name_for_ref(&input.process_ref)
        .unwrap_or("")
        .to_owned();
    let run_settings = engine.run_settings(&context)?;
    let execution_bounds = run_settings.execution_bounds;
    let (tool_catalog, host_environment) = {
        let _phase = context.named_phase("rlm_process.resolve_environment");
        let tool_catalog = match context.resolved_tool_catalog() {
            Ok(tool_catalog) => tool_catalog,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let host_environment = run_settings
            .into_surface()
            .for_process_registry(context.process_registry_available())
            .host_environment(&tool_catalog)
            .map_err(|error| error.to_string());
        if let Err(output) = validate_lashlang_process_for_run(
            &artifact,
            &input,
            LashlangHostEnvironmentCheck::CheckHostEnvironment(
                host_environment.as_ref().map_err(Clone::clone),
            ),
        ) {
            return Ok((*output).into());
        }
        let host_environment = host_environment.expect("admission accepted host environment");
        (tool_catalog, host_environment)
    };
    let process_id = context.process_id().clone();
    // The minted id is the opener: it is never reused, so no other process
    // can mint the identities this one uses (ADR 0099 §1).
    let identities = crate::LashlangHostIdentities::process_body(process_id.clone());
    let session_id = process_trace_session_id(&context.registration().provenance.originator);
    let engine_execution_id = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(|authority| authority.engine_execution_id(&process_id))
        .map(str::to_string);
    let attempt = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(lash_core::ProcessExecutionWriteAuthority::attempt)
        .expect("process engine runs with attempt-bound write authority");
    let mut lashlang_execution_trace = LashlangProcessExecutionTrace::new(
        lash_core::plugin::PluginExecutionTrace::new(context.trace_standing()),
        LashlangProcessTraceIdentity {
            session_id,
            process_id: process_id.clone(),
            source_identity: artifact.source_identity(),
            module_ref: artifact.module_ref().clone(),
            process_ref: input.process_ref.clone(),
            process_name: input.process_name.clone(),
            attempt,
            engine_execution_id,
        },
    );
    lashlang_execution_trace.execution_map =
        trace_lashlang_process_map(&artifact.graph, &input.process_name).map(Arc::new);
    if is_initial_segment {
        lashlang_execution_trace.emit_started(&artifact);
    }
    let processes = context.processes();
    // The shift's recorded cancellation fact (FIG-3673): advanced only by what
    // the engine recorded — a cancelled tool outcome, a wait the process's
    // cancellation won, a cancel checkpoint's recorded peek — so a replay
    // advances it at the same point. The engine's live stop is lent to step
    // bodies and never read here.
    let cancellation = crate::ExecutionCancellation::new();
    let (ctx, guard) = {
        let _phase = context.named_phase("rlm_process.build_context");
        let runtime_context = match context.into_runtime_context(tool_catalog) {
            Ok(runtime_context) => runtime_context,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let (ctx, guard) = runtime_context.into_parts();
        (ctx, guard)
    };
    definition_publication::publish_exports(&ctx, &artifact).await?;
    if let Some(segment_state) = segment_state.as_mut() {
        if let Some(transfer) = segment_state.tool_run.take() {
            let Some(successor) = segment else {
                return Err(segment_state::continuation_refused(
                    lash_core::tool_run::ContinuationRefusal::MissingSegmentAuthority,
                ));
            };
            if let Err(error) = ctx.restore_run_continuation(*transfer, successor) {
                return Err(segment_state::continuation_refused(error));
            }
        }
        ctx.restore_started_process_ids(&segment_state.started_process_ids);
        ctx.restore_incorporation_ledger(segment_state.incorporation_ledger.clone());
    }
    let owner = ctx.clone();
    owner
        .drive_tool_run(None, |ctx| async move {
            let ordinals = ReplayOrdinals::restore(segment_state.as_ref());
            let run = crate::LashlangReplayRun::new(
                identities.namespace(),
                ReplayOrdinals::restore_commands(segment_state.as_ref()),
            );
            let host = LashlangProcessHost {
                ctx,
                host_environment,
                artifact_store: engine.artifact_store(),
                workers: engine.workers.clone(),
                processes,
                process_id: process_id.clone(),
                identities,
                run,
                producer: serde_json::json!({
                    "compiler": lashlang::LASHLANG_COMPILER_VERSION,
                    "vm_abi": lashlang::LASHLANG_VM_ABI_VERSION,
                    "module_ref": input.module_ref.to_string(),
                }),
                lashlang_execution_trace: lashlang_execution_trace.clone(),
                ordinals,
                worker_recovery,
                cancellation: cancellation.clone(),
                host_failure: Default::default(),
                effect_summary: segment_state.as_ref().map_or_else(
                    EffectSummaryWriter::default,
                    |state| {
                        EffectSummaryWriter::restore(
                            state.pending_summary.clone(),
                            state.effect_omissions.clone(),
                        )
                    },
                ),
            };
            let output = {
                let _phase = host.ctx.named_phase("rlm_process.execute");
                execute_lashlang(
                    &engine.workers,
                    &artifact,
                    &input,
                    execution_bounds,
                    segment_controller.controller(),
                    &host,
                    (segment_state, current_program_hash),
                )
                .await
            };
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    let host_failure = host.host_failure.lock_recover().take();
                    drop(host);
                    guard
                        .shutdown(false)
                        .await
                        .map_err(lash_core::ProcessInfraError::new)?;
                    return Err(host_failure.map_or(error, lash_core::ProcessInfraError::new));
                }
            };
            // A body refused at its journal (FIG-3586) stopped where it diverged: it
            // writes nothing more — no summary, no group finalization — and its
            // refusal surfaces from the run guard below as infrastructure, so the
            // process stays non-terminal and every redrive refuses again with nothing
            // dispatched until an operator acts.
            let refused = host.ctx.nested_replay_mismatch().is_some();
            let host_failure = host.host_failure.lock_recover().take();
            let mut output = output;
            if !refused && host_failure.is_none() && output.is_terminal() {
                // A body that ends must end where the run that wrote its journal
                // ended (FIG-3586). Its terminal is the registry's to record, so it
                // journals no seal of its own.
                host.commands().close_unsealed().await;
                // The run's terminal batch (FIG-3571): its pending occurrences, then
                // its omission record, ahead of the terminal event the runner commits
                // in the same transaction.
                if let lash_core::ProcessRunOutcome::Terminal { prelude, output } = &mut output {
                    *prelude = host.effect_summary.terminal_prelude(
                        host.identities.effect_omissions(),
                        host.ctx.fleet_format(),
                    );
                    adopt_held_attachments(&host, output).await?;
                }
            }
            // A process terminal is the process opener's end (ADR 0099 §7): its
            // tool Run is closed and its accepted finals, losers' included, are
            // incorporated before the terminal is handed back to be committed. A
            // segment boundary is not an end — the successor adopts the Run the
            // handover carried. A failed close leaves `closing`
            // recorded and surfaces as infrastructure, so the run is retried rather
            // than committing a terminal whose accounting was never incorporated.
            if output.is_terminal() && !refused && host_failure.is_none() {
                let _phase = host.ctx.named_phase("rlm_process.close_groups");
                host.ctx.close_tool_run().await.map_err(|error| {
                    lash_core::ProcessInfraError::new(
                        lash_core::PluginError::RuntimeEffectController(error),
                    )
                })?;
            }
            drop(host);
            {
                let _phase = lash_core::runtime::RuntimeNamedPhase::begin(
                    phase_probe,
                    "rlm_process.shutdown",
                );
                guard
                    .shutdown(false)
                    .await
                    .map_err(lash_core::ProcessInfraError::new)?;
            }
            if let Some(fault) = host_failure {
                return Err(lash_core::ProcessInfraError::new(fault));
            }
            if output.is_terminal()
                && let Some(output) = output.terminal_output()
            {
                lashlang_execution_trace.emit_finished(output);
            }
            Ok(output)
        })
        .await
        .map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::RuntimeEffectController(
                error,
            ))
        })?
}

async fn execute_lashlang(
    workers: &lash_vm_client::service::Service,
    artifact: &lash_vm_client::InspectedArtifact,
    input: &LashlangProcessInput,
    bounds: lashlang::ExecutionBounds,
    controller: &dyn lash_core::RuntimeEffectController,
    host: &LashlangProcessHost<'_>,
    segment: (Option<LashlangSegmentState>, String),
) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
    let infra = |message: String| {
        lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(message))
    };
    let (segment_state, program_hash) = segment;
    let owner = segment_continuation_owner(&host.process_id);
    if let Some(state) = &segment_state {
        hold_segment_definitions(&host.ctx, state.vm.definition_ids()).await?;
    }
    let start = match segment_state {
        Some(state) => lash_vm_protocol::StartState::Continuation(state.vm),
        None => {
            let mut state = lash_vm_client::RemoteState::pristine(workers.clone());
            state
                .defaults(
                    input
                        .args
                        .iter()
                        .map(|(k, v)| (k.clone(), lashlang::from_json(v.clone())))
                        .collect(),
                    Default::default(),
                )
                .await
                .map_err(|error| {
                    lash_core::ProcessInfraError::new(lash_core::PluginError::Runtime(
                        error.into_runtime_error(),
                    ))
                })?;
            lash_vm_protocol::StartState::Snapshot(lash_vm_protocol::OpaqueVmState::seal(
                lash_vm_protocol::VmStateKind::Snapshot,
                owner.clone(),
                lashlang::vm_contract_versions(),
                state.bytes().unwrap_or_default().to_vec(),
            ))
        }
    };
    let progress = std::sync::Mutex::new(lash_core::SegmentProgress::default());
    let reason = std::sync::Mutex::new(None);
    let boundary = || {
        let mut progress = progress.lock_recover();
        progress.effects_executed += 1;
        let next = controller.wants_segment_boundary(&progress);
        let wanted = next.is_some();
        *reason.lock_recover() = next;
        wanted
    };
    let run = crate::WorkerRun {
        service: workers,
        host,
        identities: host.identities.code().clone(),
        owner,
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Artifact {
            module_ref: artifact.module_ref().to_string(),
            entry: lash_vm_protocol::ProgramEntry::Process {
                component: input.process_ref.component.to_string(),
                position: input.process_ref.pos,
            },
            artifact: artifact.bytes().to_vec(),
        },
        context: lash_vm_client::RunContext {
            environment: host.host_environment.clone(),
            mode: lashlang::ExecutionMode::Process,
            projected: Vec::new(),
            observe_execution: host.observes_lashlang_execution(),
            ..Default::default()
        },
        projected: lashlang::ProjectedBindings::new(),
        bounds,
        state: start,
        boundary: &boundary,
        hand_over: None,
        projection_namespace: None,
    }
    .run()
    .await;
    let run = match run {
        Ok(run) => run,
        Err(failure) => {
            return match process_worker_failure(&failure) {
                Some(terminal) => Ok(terminal.into()),
                None => Err(infra(failure.to_string())),
            };
        }
    };
    Ok(match run {
        lash_vm_broker::BrokeredEnd::Complete { value, .. } => process_lashlang_execution_result(
            Ok(rmp_serde::from_slice(&value.0).map_err(|e| infra(e.to_string()))?),
            None,
        )
        .into(),
        lash_vm_broker::BrokeredEnd::GuestError { error, .. } => process_lashlang_execution_result(
            Err(rmp_serde::from_slice::<lashlang::RuntimeFailure>(&error.0)
                .map_err(|e| infra(e.to_string()))?
                .error),
            host.ctx.tool_call_limit_refusal(),
        )
        .into(),
        lash_vm_broker::BrokeredEnd::Suspended { checkpoint } => {
            hold_segment_definitions(&host.ctx, checkpoint.vm.definition_ids()).await?;
            let boundary_reason = reason
                .lock_recover()
                .take()
                .unwrap_or(lash_core::BoundaryReason::HandOver);
            host.ctx
                .capture_tool_run(boundary_reason)
                .await
                .map_err(|error| {
                    lash_core::ProcessInfraError::new(
                        lash_core::PluginError::RuntimeEffectController(error),
                    )
                })?;
            lash_core::ProcessRunOutcome::SegmentBoundary(
                capture_segment(checkpoint.vm, host, boundary_reason, &program_hash)
                    .map_err(|(error, message)| infra(format!("{message}: {error}")))?,
            )
        }
        lash_vm_broker::BrokeredEnd::Cancelled => {
            process_lashlang_cancelled("lashlang process was cancelled").into()
        }
    })
}

/// Whose continuation a segment carries: the durable process it parks.
fn segment_continuation_owner(process_id: &ProcessId) -> lash_vm_protocol::VmOwner {
    lash_vm_protocol::VmOwner::new(format!("process:{process_id}"))
}

/// A segment continuation belongs to this process, is inside each VM
/// component's read range, and is within the size bound.
fn segment_continuation_expectation<'a>(
    owner: &'a lash_vm_protocol::VmOwner,
    reads: &'a lash_vm_protocol::VmContractReads,
) -> lash_vm_protocol::StateExpectation<'a> {
    lash_vm_protocol::StateExpectation {
        kind: lash_vm_protocol::VmStateKind::Continuation,
        owner,
        reads,
        max_bytes: MAX_SEGMENT_CONTINUATION_BYTES,
    }
}

struct LashlangProcessHost<'run> {
    ctx: lash_core::RuntimeExecutionContext<'run>,
    host_environment: lashlang::LashlangHostEnvironment,
    artifact_store: lashlang::LashlangArtifacts,
    workers: lash_vm_client::service::Service,
    processes: lash_core::facade_support::ProcessEngineProcessContext,
    process_id: ProcessId,
    /// The one derivation of the ids and key namespace this tier mints,
    /// shared with the RLM cell bridge. The authority is this process,
    /// never a segment: a body that hands over keeps minting from
    /// the scope its first segment used.
    identities: crate::LashlangHostIdentities,
    /// The run's issue-ordinal mint and recorded frontier (FIG-3586), resumed
    /// from the segment state a handover carried.
    run: crate::LashlangReplayRun,
    /// Who is running this body, for the attribution a refusal and the seal
    /// carry.
    producer: serde_json::Value,
    lashlang_execution_trace: LashlangProcessExecutionTrace,
    /// The replay ordinals this segment is consuming: restored from the
    /// handover that resumed the run (or zeroed for a first segment) and
    /// snapshotted into the next boundary's envelope.
    ordinals: ReplayOrdinals,
    /// The worker accounting the handover that resumed the run carried (or
    /// the first segment's): its boundary keys this run's reservation, and
    /// the next boundary's envelope carries it on.
    worker_recovery: WorkerRecoveryLedger,
    /// This run's recorded cancellation fact, read by the VM's cooperative
    /// cancellation probe so a cancelled process terminates as an uncatchable
    /// host terminal instead of running to completion inside a guest handler.
    /// Recorded cancellation and a failed host boundary stop the guest.
    /// A host failure is returned separately, never committed as cancellation.
    cancellation: crate::ExecutionCancellation,
    /// The durable effect summary this run writes at result incorporation.
    effect_summary: EffectSummaryWriter,
    host_failure: std::sync::Mutex<Option<lash_core::PluginError>>,
}

#[cfg(any(test, feature = "testing"))]
mod host_failure_testing;
#[cfg(any(test, feature = "testing"))]
pub(crate) use host_failure_testing::process_event_host_failure_stops_execution;

#[async_trait::async_trait]
trait SignalWaitProcesses: Send + Sync {
    async fn current_wait(&self) -> Result<Option<lash_core::WaitState>, lash_core::PluginError>;

    async fn event_page(
        &self,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_core::PluginError,
    >;

    async fn set_wait(
        &self,
        wait: lash_core::WaitState,
        prelude: Vec<lash_core::ProcessEventAppendRequest>,
    ) -> Result<(), lash_core::PluginError>;

    async fn clear_wait(
        &self,
        prelude: Vec<lash_core::ProcessEventAppendRequest>,
    ) -> Result<(), lash_core::PluginError>;

    async fn is_terminal(&self) -> Result<bool, lash_core::PluginError>;
}

#[async_trait::async_trait]
impl SignalWaitProcesses for lash_core::facade_support::ProcessEngineProcessContext {
    async fn current_wait(&self) -> Result<Option<lash_core::WaitState>, lash_core::PluginError> {
        Ok(self
            .record()
            .await?
            .and_then(|record| record.wait().cloned()))
    }

    async fn event_page(
        &self,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_core::PluginError,
    > {
        self.event_page(
            after_sequence,
            limit,
            lash_core::ProcessEventQueryMode::Full,
        )
        .await
    }

    async fn set_wait(
        &self,
        wait: lash_core::WaitState,
        prelude: Vec<lash_core::ProcessEventAppendRequest>,
    ) -> Result<(), lash_core::PluginError> {
        self.set_wait(wait, prelude).await.map(|_| ())
    }

    async fn clear_wait(
        &self,
        prelude: Vec<lash_core::ProcessEventAppendRequest>,
    ) -> Result<(), lash_core::PluginError> {
        self.clear_wait(prelude).await.map(|_| ())
    }

    async fn is_terminal(&self) -> Result<bool, lash_core::PluginError> {
        Ok(self
            .record()
            .await?
            .is_some_and(|record| record.is_terminal()))
    }
}

/// A wait-state write the registry refused because the process is already
/// terminal is settled, not failed: the process's terminal was stored (a
/// redrive after the completion step replays the body over it), and a wait
/// on a terminal process has nothing left to record (FIG-3673).
async fn settle_wait_write(
    processes: &dyn SignalWaitProcesses,
    written: Result<(), lash_core::PluginError>,
) -> Result<(), lash_core::PluginError> {
    match written {
        Ok(()) => Ok(()),
        Err(error) => match processes.is_terminal().await {
            Ok(true) => Ok(()),
            Ok(false) => Err(error),
            Err(read) => Err(read),
        },
    }
}

enum SignalWaitSetupError {
    Read(lash_core::PluginError),
    Set(lash_core::PluginError),
}

async fn establish_signal_wait(
    processes: &dyn SignalWaitProcesses,
    name: String,
    event_type: String,
    key: String,
    ordinal: u64,
    prelude: Vec<lash_core::ProcessEventAppendRequest>,
) -> Result<(), SignalWaitSetupError> {
    let since_ms = wait_since_ms(processes, &key)
        .await
        .map_err(SignalWaitSetupError::Read)?;
    let wait = lash_core::WaitState {
        since_ms,
        kind: lash_core::WaitKind::Signal {
            name,
            event_type,
            key,
            ordinal,
        },
    };
    let written = processes.set_wait(wait, prelude).await;
    settle_wait_write(processes, written)
        .await
        .map_err(SignalWaitSetupError::Set)?;
    Ok(())
}

async fn wait_since_ms(
    processes: &dyn SignalWaitProcesses,
    key: &str,
) -> Result<u64, lash_core::PluginError> {
    if let Some(since_ms) = processes
        .current_wait()
        .await?
        .and_then(|wait| match &wait.kind {
            lash_core::WaitKind::Signal { key: wait_key, .. } if wait_key == key => {
                Some(wait.since_ms)
            }
            _ => None,
        })
    {
        return Ok(since_ms);
    }

    let limit = std::num::NonZeroUsize::new(128).unwrap_or(std::num::NonZeroUsize::MIN);
    let mut after_sequence = 0;
    let mut matched_since_ms = None;
    loop {
        let outcome = processes.event_page(after_sequence, limit).await?;
        let page = match outcome {
            lash_core::ProcessEventReadOutcome::Retained(page) => page,
            lash_core::ProcessEventReadOutcome::NoLongerRetained(
                lash_core::ProcessEventHistoryRetention::Pruned {
                    terminal_label,
                    pruned_at_ms,
                },
            ) => {
                return Err(lash_core::PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                });
            }
            // The host released the prefix: a wait entered there no longer
            // tells its start, so the scan reads what is still retained.
            lash_core::ProcessEventReadOutcome::NoLongerRetained(
                lash_core::ProcessEventHistoryRetention::Released { released_through },
            ) => {
                after_sequence = released_through;
                continue;
            }
        };
        let lash_core::ProcessEventPageEvents::Full(events) = page.events else {
            unreachable!("full process event query returned a lite page");
        };
        for event in events {
            if event.event_type != "process.waiting" {
                continue;
            }
            let Some(wait_value) = event.payload.get("wait") else {
                continue;
            };
            if let Ok(wait) = serde_json::from_value::<lash_core::WaitState>(wait_value.clone())
                && wait.key() == key
            {
                matched_since_ms = Some(wait.since_ms);
            }
        }
        after_sequence = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { after_sequence } => after_sequence,
        };
    }
    Ok(matched_since_ms.unwrap_or_else(lash_core::facade_support::current_epoch_ms))
}

type ProcessHostAbilityFuture<'a> =
    lash_sansio::future::SendBoxFuture<'a, Result<lashlang::AbilityOutcome, ExecutionHostError>>;

impl LashlangProcessHost<'_> {
    fn resource_payload(
        &self,
        args: &[lashlang::Value],
    ) -> Result<serde_json::Value, ExecutionHostError> {
        let mut payload = if let [lashlang::Value::Record(record)] = args {
            lashlang_value_to_json(&lashlang::Value::Record(Arc::clone(record)))?
        } else {
            serde_json::json!({
                "args": args
                    .iter()
                    .map(lashlang_value_to_json)
                    .collect::<Result<Vec<_>, _>>()?,
            })
        };
        payload
            .as_object_mut()
            .ok_or_else(|| ExecutionHostError::from(LashlangHostError::ModulePayloadNotObject))?;
        Ok(payload)
    }

    /// The run's command protocol over this body's execution (FIG-3586).
    fn commands(&self) -> crate::ReplayCommands<'_, '_> {
        crate::ReplayCommands {
            run: &self.run,
            ctx: &self.ctx,
            cancellation: &self.cancellation,
            producer: self.producer.to_string(),
        }
    }

    async fn resource_operation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<lashlang::AbilityOutcome, ExecutionHostError> {
        let commands = self.commands();
        let command = commands.issue()?;
        if let Some(checked) = crate::language_runtime_operation(&receiver, &operation, &args) {
            let runtime_operation = match checked {
                Ok(runtime_operation) => runtime_operation,
                Err(error) => {
                    commands.skipped(&command)?;
                    return Err(error);
                }
            };
            let in_flight = commands.enter(command, crate::CommandShape::Value).await?;
            let key = in_flight.command.key.as_str().to_string();
            let result = self
                .language_runtime_value(&in_flight, runtime_operation, call_site.as_ref(), key)
                .await;
            commands.finish(&in_flight)?;
            return result.map(lashlang::AbilityOutcome::Value);
        }
        let call_id = self.identities.call_id(command.ordinal);
        let prepared = match self.prepare_resource_invocation(
            lashlang::ResourceOperation {
                operation,
                receiver,
                args,
                call_site,
            },
            call_id,
            command.key.as_str().to_string(),
            None,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        match prepared {
            PreparedResourceInvocation::Trigger {
                operation,
                payload,
                call,
            } => {
                let in_flight = commands.enter(command, crate::CommandShape::Value).await?;
                let result = self
                    .trigger_operation(
                        &in_flight.ctx,
                        operation,
                        payload,
                        call.journal_key,
                        &call.host_operation,
                        Some(&call.call_site),
                    )
                    .await;
                commands.finish(&in_flight)?;
                result.map(lashlang::AbilityOutcome::Value)
            }
            PreparedResourceInvocation::Tool { invocation, call } => {
                // The call's journal rows live under its command key; that is
                // the replay key its summary and its failures name.
                let replay_key = call.journal_key;
                let in_flight = commands
                    .enter(command, crate::CommandShape::ToolCall)
                    .await?;
                let reply = Box::pin(
                    in_flight
                        .ctx
                        .call_command_tool(&in_flight.command.key, invocation),
                )
                .await;
                if in_flight.ctx.take_wait_handed_over() {
                    commands.hand_over(&in_flight)?;
                    return Ok(lashlang::AbilityOutcome::HandedOver);
                }
                commands.finish(&in_flight)?;
                self.record_tool_reply(&call.call_site, &call.host_operation, &replay_key, &reply);
                protocol_tool_reply_to_lashlang_value(reply, &replay_key, &self.cancellation)
                    .map(lashlang::AbilityOutcome::Value)
            }
        }
    }

    async fn await_handle(
        &self,
        handle: lashlang::Value,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let commands = self.commands();
        let command = commands.issue()?;
        let handle = match lashlang_value_to_json(&handle) {
            Ok(handle) => handle,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let call_id = self.identities.call_id(command.ordinal);
        let in_flight = commands
            .enter(command, crate::CommandShape::AwaitHandle)
            .await?;
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.await_handle");
            in_flight
                .ctx
                .under_command(&in_flight.command.key)
                .await_tool_handle(call_id.clone(), handle)
                .await
        };
        commands.finish(&in_flight)?;
        protocol_tool_reply_to_lashlang_value(reply, call_id.as_str(), &self.cancellation)
    }

    async fn process_event(&self, event: lashlang::ProcessEvent) -> Result<(), ExecutionHostError> {
        let event_type = match event.kind {
            lashlang::ProcessEventKind::Yield => "process.yield",
            lashlang::ProcessEventKind::Wake => "process.wake",
        };
        // An event holds an issue ordinal like every command, but it is an
        // append to the process's event log, addressed by its own sequence,
        // and writes nothing to the effect journal.
        let commands = self.commands();
        let command = commands.issue()?;
        let in_flight = commands.enter(command, crate::CommandShape::Silent).await?;
        let ordinal = self.ordinals.event_sequence.fetch_add(1, Ordering::Relaxed);
        // The body's event is a run boundary: the run's pending summary
        // commits ahead of it in the same batch (FIG-3571).
        let summary = self.effect_summary.prelude();
        let mut batch = summary.requests.clone();
        batch.push(
            lash_core::ProcessEventAppendRequest::new(
                event_type,
                process_event_payload(&event.value)?,
            )
            .with_replay_key(format!("process:{}:event:{ordinal}", self.process_id)),
        );
        let appended = self.ctx.append_process_events(batch).await.map(|_| ());
        commands.finish(&in_flight)?;
        self.settle_boundary(&summary, appended, ProcessHostOp::AppendProcessEvent)?;
        Ok(())
    }

    async fn sleep(&self, sleep: lashlang::Sleep) -> Result<lashlang::Value, ExecutionHostError> {
        let commands = self.commands();
        let command = commands.issue()?;
        let call_site = sleep.call_site;
        let sleep = match process_sleep(sleep.kind, &sleep.value) {
            Ok(sleep) => sleep,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let in_flight = commands.enter(command, crate::CommandShape::Sleep).await?;
        if let Some(call_site) = &call_site {
            self.lashlang_execution_trace.emit_waiting(
                call_site,
                TraceNodeAwaited::Sleep {
                    deadline_ms: match sleep {
                        lash_core::SleepSpec::Until { deadline_ms } => Some(deadline_ms),
                        lash_core::SleepSpec::For { .. } => None,
                    },
                },
            );
        }
        let slept = in_flight
            .ctx
            .sleep_command(&in_flight.command.key, sleep)
            .await;
        commands.finish(&in_flight)?;
        // The effect host journals the wake verdict, so a success or a
        // recorded cancellation is replay-stable; any other error has no
        // recorded outcome to summarise.
        let outcome_class = match &slept {
            Ok(()) => Some(lash_core::ProcessEffectOutcomeClass::Success),
            Err(error)
                if error.code == lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled =>
            {
                // The process's cancellation won the recorded race with the
                // timer: the shift's fact advances here, at the same point on
                // every replay.
                self.cancellation.cancel();
                Some(lash_core::ProcessEffectOutcomeClass::Cancelled)
            }
            Err(_) => None,
        };
        if let (Some(call_site), Some(outcome_class)) = (&call_site, outcome_class) {
            self.record_effect_outcome(
                call_site,
                lash_core::runtime::PROCESS_SLEEP_OPERATION,
                outcome_class,
                None,
                &in_flight.command.key.sleep(),
            );
        }
        slept.map_err(|error| {
            commands.journal_error(&in_flight, error, |error| {
                if error.code == lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled {
                    LashlangHostError::HostBoundary {
                        op: ProcessHostOp::SleepProcess,
                        source: lash_core::PluginError::RuntimeEffectController(error),
                    }
                    .into()
                } else {
                    self.controller_boundary_error(ProcessHostOp::SleepProcess, error)
                }
            })
        })?;
        if let Some(call_site) = &call_site
            && !self.cancellation.is_cancelled()
        {
            self.lashlang_execution_trace
                .emit_resumed(call_site, TraceNodeWaitResolution::TimedOut);
        }
        Ok(lashlang::Value::Null)
    }

    async fn wait_signal(
        &self,
        name: String,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<lashlang::AbilityOutcome, ExecutionHostError> {
        let commands = self.commands();
        let command = commands.issue()?;
        let event_type = match lash_core::facade_support::process_signal_event_type(&name) {
            Ok(event_type) => event_type,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(self.host_boundary_error(ProcessHostOp::ValidateSignalName, error));
            }
        };
        let in_flight = commands
            .enter(command, crate::CommandShape::SignalWait)
            .await?;
        let event_ordinal = {
            let mut wait_ordinals = self.ordinals.signal_wait_ordinals.lock_recover();
            let ordinal = wait_ordinals.entry(name.clone()).or_insert(0);
            *ordinal += 1;
            *ordinal
        };
        let key = lash_core::facade_support::process_signal_wait_key(
            &self.process_id,
            &name,
            event_ordinal,
        );
        // The wait-state write is a step the engine records: a redrive after
        // the terminal is stored replays its answer instead of meeting a
        // registry that refuses a terminal process's wait (FIG-3673). It is a
        // run boundary, so the run's pending summary commits with it
        // (FIG-3571).
        let processes = self.processes.clone();
        let step_key = key.clone();
        let step_name = name.clone();
        let summary = self.effect_summary.prelude();
        let prelude = summary.requests.clone();
        let entered =
            self.ctx
                .record_process_drive_step(
                    format!("lash.process.wait.enter:{key}"),
                    Box::pin(async move {
                        establish_signal_wait(
                            &processes,
                            step_name,
                            event_type,
                            step_key,
                            event_ordinal,
                            prelude,
                        )
                        .await
                        .map_err(|error| match error {
                            SignalWaitSetupError::Read(error)
                            | SignalWaitSetupError::Set(error) => error,
                        })
                    }),
                )
                .await;
        self.settle_boundary(
            &summary,
            entered.map_err(lash_core::PluginError::RuntimeEffectController),
            ProcessHostOp::SetSignalWait,
        )?;
        if let Some(call_site) = &call_site {
            self.lashlang_execution_trace.emit_waiting(
                call_site,
                TraceNodeAwaited::Signal {
                    name: name.clone(),
                    key: key.clone(),
                },
            );
        }
        let payload = in_flight
            .ctx
            .await_process_signal_event(
                &in_flight.command.key,
                &self.process_id,
                &name,
                event_ordinal,
            )
            .await;
        commands.finish(&in_flight)?;
        if matches!(&payload, Err(error)
            if error.code == lash_core::RuntimeErrorCode::ProcessSignalWaitHandedOver)
        {
            // The drain woke this segment to hand its wait to a successor on
            // the newest build (FIG-3799): the wait stays open, its wait
            // state stays armed, and the successor issues it again under the
            // same per-name ordinal, so it waits on the same key and the
            // signal that resolves it is neither lost nor seen twice.
            let mut wait_ordinals = self.ordinals.signal_wait_ordinals.lock_recover();
            if let Some(ordinal) = wait_ordinals.get_mut(&name) {
                *ordinal = ordinal.saturating_sub(1);
            }
            return Ok(lashlang::AbilityOutcome::HandedOver);
        }
        if matches!(&payload, Err(error)
            if error.code == lash_core::RuntimeErrorCode::ProcessSignalWaitCancelled)
        {
            // The recorded wait ended cancelled: the process's cancellation
            // won its race, or its wait was cancelled for it.
            self.cancellation.cancel();
        }
        let payload = payload.map_err(|error| {
            commands.journal_error(&in_flight, error, |error| {
                self.controller_boundary_error(ProcessHostOp::AwaitSignal, error)
            })
        })?;
        let processes = self.processes.clone();
        let summary = self.effect_summary.prelude();
        let prelude = summary.requests.clone();
        let cleared = self
            .ctx
            .record_process_drive_step(
                format!("lash.process.wait.clear:{key}"),
                Box::pin(async move {
                    let cleared = SignalWaitProcesses::clear_wait(&processes, prelude).await;
                    settle_wait_write(&processes, cleared).await
                }),
            )
            .await;
        self.settle_boundary(
            &summary,
            cleared.map_err(lash_core::PluginError::RuntimeEffectController),
            ProcessHostOp::ClearSignalWait,
        )?;
        if let Some(call_site) = &call_site
            && !self.cancellation.is_cancelled()
        {
            self.lashlang_execution_trace
                .emit_resumed(call_site, TraceNodeWaitResolution::Resumed);
        }
        Ok(lashlang::AbilityOutcome::Value(lashlang::from_json(
            payload,
        )))
    }

    fn perform_selected_ability<'a>(
        &'a self,
        op: lashlang::AbilityOp,
    ) -> ProcessHostAbilityFuture<'a> {
        match op {
            lashlang::AbilityOp::ResourceOperation(operation) => Box::pin(async move {
                Box::pin(self.resource_operation(
                    operation.operation,
                    operation.receiver,
                    operation.args,
                    operation.call_site,
                ))
                .await
            }),
            lashlang::AbilityOp::ResourceOperationBatch(batch) => {
                Box::pin(async move { Box::pin(self.resource_operation_batch(batch)).await })
            }
            lashlang::AbilityOp::Await(handle) => Box::pin(async move {
                self.await_handle(handle)
                    .await
                    .map(lashlang::AbilityOutcome::Value)
            }),
            lashlang::AbilityOp::ProcessEvent(event) => Box::pin(async move {
                self.process_event(event).await?;
                Ok(lashlang::AbilityOutcome::Unit)
            }),
            lashlang::AbilityOp::Sleep(sleep) => {
                Box::pin(
                    async move { self.sleep(sleep).await.map(lashlang::AbilityOutcome::Value) },
                )
            }
            lashlang::AbilityOp::WaitSignal { name, call_site } => {
                Box::pin(async move { self.wait_signal(name, call_site).await })
            }
            lashlang::AbilityOp::Print(_) => {
                Box::pin(async { Err(LashlangHostError::PrintUnavailable.into()) })
            }
            lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(lashlang::AbilityOutcome::Value(value)) })
            }
        }
    }
}

impl lashlang::ExecutionHost for LashlangProcessHost<'_> {
    fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> impl Future<Output = Result<lashlang::AbilityOutcome, ExecutionHostError>> + Send {
        self.perform_selected_ability(op)
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// The body's cancel checkpoint (FIG-3673): the engine's recorded peek of
    /// the process's cancellation, at an instruction count a replay reaches
    /// again. A peek the controller refuses — a replay divergence among them —
    /// ends the body like any nested effect it refused.
    async fn cancel_checkpoint(&self, _checkpoint: u64) {
        if self.cancellation.is_cancelled() {
            return;
        }
        match self.ctx.process_cancel_checkpoint().await {
            Ok(false) => {}
            Ok(true) => self.cancellation.cancel(),
            Err(error) => {
                self.ctx.record_nested_effect_error(error);
                self.cancellation.cancel();
            }
        }
    }

    fn observes_lashlang_execution(&self) -> bool {
        true
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        let observation = match observation {
            lashlang::LashlangExecutionObservation::NodeFailed {
                site, occurrence, ..
            } if self.cancellation.is_cancelled()
                && self
                    .lashlang_execution_trace
                    .active_nodes
                    .lock_recover()
                    .contains_key(&(site.node_id.clone(), site.node_kind, occurrence)) =>
            {
                self.lashlang_execution_trace
                    .emit_cancelled_site(site, occurrence);
                return;
            }
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence }
                if self.cancellation.is_cancelled()
                    && self.lashlang_execution_trace.waiting_nodes.is_waiting(
                        &site.node_id,
                        site.node_kind,
                        occurrence,
                    ) =>
            {
                self.lashlang_execution_trace
                    .emit_cancelled_site(site, occurrence);
                return;
            }
            observation => observation,
        };
        self.lashlang_execution_trace.emit_observation(observation);
    }
}

#[derive(Clone)]
struct LashlangProcessExecutionTrace {
    tracing: lash_core::plugin::PluginExecutionTrace,
    session_id: Option<SessionId>,
    process_id: ProcessId,
    source_identity: String,
    module_ref: lashlang::ModuleRef,
    process_ref: lashlang::ProcessRef,
    process_name: String,
    attempt: u32,
    engine_execution_id: Option<String>,
    resource_call_ids: Arc<std::sync::Mutex<BTreeMap<(String, u64), lash_core::ToolCallId>>>,
    pending_resource_starts:
        Arc<std::sync::Mutex<BTreeMap<(String, u64), lashlang::LashlangExecutionSite>>>,
    active_nodes: Arc<std::sync::Mutex<ActiveProcessTraceNodes>>,
    waiting_nodes: crate::TraceWaitBookkeeping,
    execution_map: Option<Arc<lash_trace::TraceLanguageExecutionMap>>,
}

type ProcessTraceNodeKey = (String, lash_sansio::ExecutionNodeKind, u64);
type ActiveProcessTraceNodes = BTreeMap<ProcessTraceNodeKey, lashlang::LashlangExecutionSite>;

struct LashlangProcessTraceIdentity {
    session_id: Option<SessionId>,
    process_id: ProcessId,
    source_identity: String,
    module_ref: lashlang::ModuleRef,
    process_ref: lashlang::ProcessRef,
    process_name: String,
    attempt: u32,
    engine_execution_id: Option<String>,
}

#[path = "process/execution_trace.rs"]
mod execution_trace;

#[path = "process/terminal_attachments.rs"]
mod terminal_attachments;
use terminal_attachments::adopt_held_attachments;

fn process_trace_session_id(originator: &lash_core::ProcessOriginator) -> Option<SessionId> {
    match originator {
        lash_core::ProcessOriginator::Session { session_id, .. } => Some(session_id.clone()),
        lash_core::ProcessOriginator::Host { .. } => None,
    }
}

fn process_lashlang_cancelled(message: impl Into<String>) -> lash_core::ProcessAwaitOutput {
    lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime(message),
    ))
}

#[path = "process/event_types.rs"]
mod event_types;
pub use event_types::{lashlang_process_event_types, lashlang_process_signal_event_types};

#[path = "process/effect_operations.rs"]
mod effect_operations;
use effect_operations::EffectSummaryWriter;
#[path = "process/schema.rs"]
mod schema;
pub use schema::lashlang_type_expr_schema;

#[path = "process/trace_map.rs"]
mod trace_map;
use trace_map::language_event_node_id;
pub use trace_map::{
    TraceLanguageExecutionMapError, trace_lashlang_main_map, trace_lashlang_process_map,
    trace_lashlang_process_map_snapshot,
};

#[path = "process/resource_invocation.rs"]
mod resource_invocation;

#[path = "process/worker_recovery.rs"]
mod worker_recovery;
use resource_invocation::PreparedResourceInvocation;
use worker_recovery::WorkerRecoveryLedger;

#[cfg(test)]
#[path = "process/segment_trace_tests.rs"]
pub(crate) mod segment_trace_tests;
#[cfg(test)]
#[path = "process/signal_wait_tests.rs"]
mod signal_wait_tests;

#[cfg(test)]
#[path = "process/opaque_state_tests.rs"]
mod opaque_state_tests;

#[cfg(test)]
#[path = "process/worker_recovery_tests.rs"]
mod worker_recovery_tests;

mod definition_publication;
