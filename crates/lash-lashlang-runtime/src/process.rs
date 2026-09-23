use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_core::facade_support::ToolChildExecutionTraceHook;
use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceBranchSelection, TraceContext, TraceEvent, TraceLanguageChildExecution,
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus, TraceNodeAwaited, TraceNodeWaitResolution, TraceRecord,
    TraceRuntimeScope, TraceRuntimeSubject, TraceSink,
};
use lashlang::{ExecutionHost, ExecutionHostError};

use crate::{
    LASHLANG_ENGINE_KIND, LashlangHostEnvironmentCheck, LashlangHostError, LashlangProcessEngine,
    LashlangProcessFailureCode, LashlangProcessInput,
    bridge::{
        lashlang_value_to_json, process_event_payload, process_sleep,
        protocol_tool_reply_to_lashlang_value,
    },
    resolve_lashlang_module_operation, validate_lashlang_process_admission,
};

static SEGMENT_BOUNDARY_DECLINED_TOTAL: AtomicU64 = AtomicU64::new(0);
#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

fn record_segment_boundary_decline(error: &dyn std::fmt::Display, message: &'static str) {
    let declined_total = SEGMENT_BOUNDARY_DECLINED_TOTAL
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    tracing::warn!(error = %error, declined_total, "{message}");
}

/// Version of the durable Lashlang segment-handover envelope.
///
/// v12 carries VM continuation v16, which counts aggregates in the occurrence
/// counters this envelope hands to the next segment. A segment parked by v11
/// holds counts for execution sites only, so the batches it re-derives after
/// handover would mint ordinals its own journal never recorded. The boundary is
/// a version rather than a decode failure for the same reason v9's was: the
/// bytes still parse.
/// v11 drops `signal_send_sequence`: its only producer was deleted with the
/// signal special forms (FIG-2999), and the ordinal had been round-tripping
/// dead since, so the envelope was version-gating a field that carried no
/// meaning. The remaining ordinals move into one [`ReplayOrdinalsState`]
/// group, so the envelope, the restore path and the boundary snapshot spell
/// them once.
/// v10 drops the parent-end action list: child lifecycle is settled from the
/// registry's scope-keyed parent-end ledger, so a parked segment no longer
/// carries per-child actions a replay would have to reconcile. v8 was reserved
/// for this change and went unused: the TypeScript cutover landed first and
/// took the next generation, so this one takes the one after it.
/// v9 carries VM continuation v14. TypeScript is the only RLM language
/// (ADR 0096), so the instruction set loses the deep-copy instructions the
/// retired surface compiled to: a segment parked before the cutover holds a
/// continuation over an instruction stream this reader cannot reproduce, so the
/// boundary is a version rather than a decode failure.
/// v7 pins the attempt bound this segment stamps onto the children it starts,
/// so a redrive after a host config change re-registers the recorded bound
/// instead of conflicting with the fingerprint the first attempt wrote.
/// v13 carries the once-only incorporation ledger (FIG-3411, ADR 0099 §6/§13):
/// which settlements the opener already applied and which usage deltas it
/// already charged. A segment parked by an older version has no ledger to
/// hand over, so the boundary is a version rather than a defaulted field — a
/// defaulted empty ledger would let the successor incorporate the same
/// settlement twice and double-charge its spend.
/// v14 replaces runtime occurrence-counter keys with the shared workflow node
/// identity and reserves the process root for the process declaration.
/// v15 carries sleep/signal call-site continuations and the closed workflow
/// execution-site kind. A parked v14 segment is refused before VM restore.
/// v16 carries the durable effect summary's per-node omission counts
/// (FIG-3464): a successor counting from zero would under-report omitted
/// occurrences in the run's terminal omission record.
/// v6 carries run-local child possession across execution segments. A segment
/// parked by another version is refused rather than decoded (ADR 0055).
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store.
pub const LASHLANG_SEGMENT_STATE_VERSION: u32 = 16;

const SEGMENT_STATE_CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

#[derive(Debug, thiserror::Error)]
enum LashlangSegmentStateError {
    #[error(
        "lashlang segment handover format is incompatible: {details}; {SEGMENT_STATE_CUTOVER_REMEDY}"
    )]
    FormatMismatch { details: String },
    #[error(
        "lashlang segment handover version {found} is incompatible with version {expected}; {SEGMENT_STATE_CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
}

#[derive(serde::Deserialize)]
struct LashlangSegmentStateVersionProbe {
    version: Option<u32>,
}

/// The replay ordinals a segment hands to the next execution of its run, as
/// they sit on the wire.
///
/// One group spelled once: the envelope embeds it flattened,
/// [`ReplayOrdinals::restore`] lifts it into the run's live counters and
/// [`ReplayOrdinals::snapshot`] writes it back. A counter spelled at fewer
/// than all three sites used to compile — `signal_send_sequence` kept
/// round-tripping for a day after FIG-2999 deleted its only producer.
#[derive(serde::Serialize, serde::Deserialize)]
struct ReplayOrdinalsState {
    sleep_sequence: u64,
    event_sequence: u64,
    signal_wait_ordinals: BTreeMap<String, u64>,
}

/// The live counterpart of [`ReplayOrdinalsState`]: the ordinals the running
/// segment is consuming, held as the counters the host mutates in place.
struct ReplayOrdinals {
    sleep_sequence: AtomicU64,
    event_sequence: AtomicU64,
    signal_wait_ordinals: tokio::sync::Mutex<BTreeMap<String, u64>>,
}

impl ReplayOrdinals {
    fn restore(state: Option<&LashlangSegmentState>) -> Self {
        let ordinals = state.map(|state| &state.ordinals);
        Self {
            sleep_sequence: AtomicU64::new(ordinals.map_or(0, |o| o.sleep_sequence)),
            event_sequence: AtomicU64::new(ordinals.map_or(0, |o| o.event_sequence)),
            signal_wait_ordinals: tokio::sync::Mutex::new(
                ordinals.map_or_else(BTreeMap::new, |o| o.signal_wait_ordinals.clone()),
            ),
        }
    }

    async fn snapshot(&self) -> ReplayOrdinalsState {
        ReplayOrdinalsState {
            sleep_sequence: self.sleep_sequence.load(Ordering::Relaxed),
            event_sequence: self.event_sequence.load(Ordering::Relaxed),
            signal_wait_ordinals: self.signal_wait_ordinals.lock().await.clone(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LashlangSegmentState {
    version: u32,
    vm: lashlang::VmContinuation,
    #[serde(flatten)]
    ordinals: ReplayOrdinalsState,
    started_process_ids: Vec<ProcessId>,
    /// Attempt bound resolved from the host config when this run's first
    /// segment began. Carried forward so every segment of the run, and every
    /// redrive of it, registers children with the same recorded value.
    child_max_attempts: std::num::NonZeroU32,
    /// The once-only settlement incorporation ledger (FIG-3411): a successor
    /// segment incorporates against the same set so a redrive cannot
    /// re-apply a settlement or re-charge a usage delta.
    incorporation_ledger: lash_core::session::IncorporationLedger,
    /// Effect occurrences past the durable summary's per-node cap, counted by
    /// outcome class (FIG-3464). A successor segment keeps counting from here
    /// and the run's terminal omission record carries the total.
    effect_omissions: BTreeMap<String, lash_core::ProcessEffectOmittedCounts>,
}

/// A segment that resumes carries the bound its first segment recorded, so a
/// redrive after the host's default changes re-registers every child with the
/// value already hashed into its registration fingerprint rather than
/// conflicting against it. Only a first segment reads the live host default.
fn resolve_child_max_attempts(
    segment_state: Option<&LashlangSegmentState>,
    host_default: std::num::NonZeroU32,
) -> std::num::NonZeroU32 {
    segment_state.map_or(host_default, |state| state.child_max_attempts)
}

fn decode_lashlang_segment_state(
    data: &[u8],
) -> Result<LashlangSegmentState, LashlangSegmentStateError> {
    let probe: LashlangSegmentStateVersionProbe =
        serde_json::from_slice(data).map_err(|error| {
            LashlangSegmentStateError::FormatMismatch {
                details: error.to_string(),
            }
        })?;
    let found = probe.version.unwrap_or(0);
    if found != LASHLANG_SEGMENT_STATE_VERSION {
        return Err(LashlangSegmentStateError::VersionMismatch {
            expected: LASHLANG_SEGMENT_STATE_VERSION,
            found,
        });
    }
    serde_json::from_slice(data).map_err(|error| LashlangSegmentStateError::FormatMismatch {
        details: error.to_string(),
    })
}

/// The durable program identity a Lashlang process resumes against.
///
/// Public because a readability preflight has no other way to ask the bytecode
/// question. The identity is a hash whose preimage includes
/// [`lashlang::BYTECODE_FORMAT_VERSION`], so nothing stored says "this was
/// compiled by version N": the only check available is to recompute the
/// identity this build would mint for the same inputs and compare it against
/// the one the process recorded. That is why the format manifest classifies
/// bytecode as identity-only rather than comparable.
#[expect(
    clippy::expect_used,
    reason = "the identity is a tuple of strings and integer constants serialized straight to in-memory bytes"
)]
pub fn lashlang_program_hash(input: &LashlangProcessInput) -> String {
    let identity = serde_json::to_vec(&(
        "lashlang-bytecode",
        lashlang::BYTECODE_FORMAT_VERSION,
        &input.module_ref,
        &input.process_ref,
        &input.host_requirements_ref,
        &input.process_name,
    ))
    .expect("lashlang program identity should serialize");
    format!(
        "blake3:{}",
        lash_sansio::core_support::blake3_domain_hash_hex("lash-lashlang-program/v2", identity,)
    )
}

fn validate_lashlang_program_hash(
    persisted: &str,
    current: &str,
) -> Result<(), Box<lash_core::ProcessAwaitOutput>> {
    if persisted != current {
        return Err(Box::new(process_lashlang_failure(
            LashlangProcessFailureCode::RestateSegmentProgramHashMismatch,
            format!(
                "lashlang bytecode v{} segment program identity mismatch: persisted {persisted}, current {current}",
                lashlang::BYTECODE_FORMAT_VERSION
            ),
            None,
        )));
    }
    Ok(())
}

pub(crate) fn validate_lashlang_process_for_run(
    artifact: &lashlang::ModuleArtifact,
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

#[expect(
    clippy::expect_used,
    reason = "admission accepted the host environment, which the message states and the raw_host_environment branch above establishes"
)]
pub async fn run_lashlang_process(
    engine: LashlangProcessEngine,
    mut context: lash_core::ProcessEngineRunContext<'_>,
    payload: serde_json::Value,
) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
    let handover = context.take_handover();
    let is_initial_segment = handover.is_none();
    let persisted_program_hash = handover
        .as_ref()
        .map(|handover| handover.program_hash.clone());
    let segment_controller = context.scoped_effect_controller();
    let phase_probe = context.turn_phase_probe();
    let input = match LashlangProcessInput::from_payload(payload) {
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
    let artifact = {
        let _phase = context.named_phase("rlm_process.load_artifact");
        match engine
            .artifact_store
            .get_module_artifact(&input.module_ref)
            .await
        {
            Ok(Some(artifact)) => artifact,
            Ok(None) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessModuleArtifactMissing,
                    format!("missing lashlang module artifact `{}`", input.module_ref),
                    None,
                )
                .into());
            }
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(
                    lash_core::PluginError::Session(format!(
                        "failed to load lashlang module artifact `{}`: {err}",
                        input.module_ref
                    )),
                ));
            }
        }
    };
    let (tool_catalog, host_environment) = {
        let _phase = context.named_phase("rlm_process.resolve_environment");
        let tool_catalog = match context.resolved_tool_catalog() {
            Ok(tool_catalog) => tool_catalog,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let session_extensions = context.plugins().session_extensions().clone();
        let surface = engine
            .surface
            .clone()
            .for_process_registry(context.process_registry_available())
            .with_plugin_extensions(&session_extensions);
        let host_environment = match surface {
            Ok(surface) => surface
                .host_environment(&tool_catalog)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
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
    let compiled = {
        let _phase = context.named_phase("rlm_process.compile");
        let compiled = engine.process_cache.lock_recover().get_or_compile(
            &artifact,
            &input.process_ref,
            &input.host_requirements_ref,
        );
        match compiled {
            Ok(compiled) => compiled,
            Err(err) => {
                return Ok(process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessCompileFailed,
                    format!("failed to compile process `{}`: {err}", input.process_name),
                    None,
                )
                .into());
            }
        }
    };
    let current_program_hash = lashlang_program_hash(&input);
    if let Some(persisted_program_hash) = persisted_program_hash
        && let Err(output) =
            validate_lashlang_program_hash(&persisted_program_hash, &current_program_hash)
    {
        return Ok((*output).into());
    }
    let segment_state: Option<LashlangSegmentState> = match handover {
        Some(handover) => match decode_lashlang_segment_state(&handover.engine_state) {
            Ok(state) => Some(state),
            Err(err) => {
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
    let process_id = context.registration().id.clone();
    // The opener, not the name: a process re-registered under the same name is
    // a different opener and must never mint identities the predecessor used
    // (ADR 0099 §1).
    let identities = crate::LashlangHostIdentities::process_body(lash_core::ProcessRef::new(
        process_id.clone(),
        context.incarnation(),
    ));
    let session_id = process_trace_session_id(&context.registration().provenance.originator);
    let restate_invocation_id = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(|authority| authority.restate_invocation_id(&process_id))
        .map(str::to_string);
    let attempt = context
        .execution_context()
        .execution_write_authority
        .as_ref()
        .and_then(lash_core::ProcessExecutionWriteAuthority::attempt)
        .expect("process engine runs with attempt-bound write authority");
    let mut lashlang_execution_trace = LashlangProcessExecutionTrace::new(
        engine.execution_sink.clone(),
        engine.trace_context.clone(),
        LashlangProcessTraceIdentity {
            session_id,
            process_id: process_id.clone(),
            source_identity: trace_lashlang_source_identity(&artifact),
            module_ref: artifact.module_ref.clone(),
            process_ref: input.process_ref.clone(),
            process_name: input.process_name.clone(),
            attempt,
            incarnation: context.incarnation(),
            restate_invocation_id,
        },
    );
    lashlang_execution_trace.execution_map =
        trace_lashlang_process_map(&artifact, &input.process_name).map(Arc::new);
    if is_initial_segment {
        lashlang_execution_trace.emit_started(&artifact);
    }
    let processes = context.processes();
    // The run's own cancellation scope: cancelled when the engine cancels this
    // process, and also when the run itself observes a terminal the guest may
    // not catch — a cancelled tool call.
    let cancellation = crate::ExecutionCancellation::child_of(&context.cancellation_token());
    let (ctx, guard, mut state) = {
        let _phase = context.named_phase("rlm_process.build_context");
        let runtime_context = match context.into_runtime_context(tool_catalog) {
            Ok(runtime_context) => runtime_context,
            Err(err) => {
                return Err(lash_core::ProcessInfraError::new(err));
            }
        };
        let (ctx, guard) = runtime_context.into_parts();
        let mut globals = lashlang::Record::with_capacity(input.args.len());
        for (name, value) in input.args {
            globals.insert(name, lashlang::from_json(value));
        }
        let state = lashlang::State::from_snapshot(lashlang::Snapshot::new(globals));
        (ctx, guard, state)
    };
    if let Some(segment_state) = segment_state.as_ref() {
        ctx.restore_started_process_ids(&segment_state.started_process_ids);
        ctx.restore_incorporation_ledger(segment_state.incorporation_ledger.clone());
    }
    let ordinals = ReplayOrdinals::restore(segment_state.as_ref());
    let child_max_attempts =
        resolve_child_max_attempts(segment_state.as_ref(), ctx.engine_child_max_attempts());
    let host = LashlangProcessHost {
        ctx,
        host_environment,
        artifact_store: engine.artifact_store(),
        processes,
        process_id: process_id.clone(),
        identities,
        lashlang_execution_trace: lashlang_execution_trace.clone(),
        ordinals,
        child_max_attempts,
        cancellation: cancellation.clone(),
        effect_summary: EffectSummaryWriter::restore(
            segment_state
                .as_ref()
                .map(|state| state.effect_omissions.clone())
                .unwrap_or_default(),
        ),
    };
    let env = lashlang::ExecutionEnvironment::new(&host)
        .process()
        .with_execution_bounds(engine.execution_bounds);
    let output = {
        let _phase = host.ctx.named_phase("rlm_process.execute");
        execute_lashlang(
            compiled,
            &mut state,
            &env,
            cancellation.clone(),
            segment_controller.controller(),
            &host,
            (segment_state, current_program_hash),
        )
        .await
    };
    // The omission record precedes the terminal event the runner appends
    // next; a run that failed an incorporation writes nothing more.
    let mut incorporation_fault = host.effect_summary.take_incorporation_fault();
    if incorporation_fault.is_none() && output.is_terminal() {
        host.record_effect_omissions().await;
        incorporation_fault = host.effect_summary.take_incorporation_fault();
    }
    drop(env);
    drop(host);
    {
        let _phase =
            lash_core::runtime::RuntimeNamedPhase::begin(phase_probe, "rlm_process.shutdown");
        guard
            .shutdown(false)
            .await
            .map_err(lash_core::ProcessInfraError::new)?;
    }
    if let Some(fault) = incorporation_fault {
        return Err(lash_core::ProcessInfraError::new(fault));
    }
    if output.is_terminal()
        && let Some(output) = output.terminal_output()
    {
        lashlang_execution_trace.emit_finished(output);
    }
    Ok(output)
}

async fn execute_lashlang(
    compiled: Arc<lashlang::CompiledProgram>,
    state: &mut lashlang::State,
    env: &lashlang::ExecutionEnvironment<'_, LashlangProcessHost<'_>>,
    cancellation: crate::ExecutionCancellation,
    controller: &dyn lash_core::RuntimeEffectController,
    host: &LashlangProcessHost<'_>,
    segment: (Option<LashlangSegmentState>, String),
) -> lash_core::ProcessRunOutcome {
    let (segment_state, program_hash) = segment;
    let mut vm = if let Some(segment_state) = segment_state {
        match lashlang::Vm::resume_from(segment_state.vm, compiled.as_ref(), env) {
            Ok(vm) => vm,
            Err(err) => {
                let exhausted = err.is_execution_bound_exhausted();
                #[cfg(any(test, feature = "testing"))]
                assert!(
                    !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst) || !exhausted,
                    "confidence durable process exhausted a required Lashlang bound: {err}"
                );
                return process_lashlang_failure(
                    if exhausted {
                        LashlangProcessFailureCode::ProcessExecutionBoundExhausted
                    } else {
                        LashlangProcessFailureCode::ProcessSegmentResumeFailed
                    },
                    format!("failed to resume lashlang segment: {err}"),
                    None,
                )
                .into();
            }
        }
    } else {
        match lashlang::Vm::from_state(compiled.as_ref(), state, env) {
            Ok(vm) => vm,
            Err(err) => {
                return process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessSegmentResumeFailed,
                    format!("failed to install lashlang snapshot: {err}"),
                    None,
                )
                .into();
            }
        }
    };
    let mut progress = lash_core::SegmentProgress::default();
    loop {
        let execution = if env.trace_runtime_errors() {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return process_lashlang_cancelled("lashlang process was cancelled").into();
                }
                result = vm.run_process_traced_until_effect() => {
                    result.map_err(|failure| {
                        let error = failure.error.clone();
                        env.observe_runtime_failure(failure);
                        error
                    })
                }
            }
        } else {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return process_lashlang_cancelled("lashlang process was cancelled").into();
                }
                result = vm.run_process_until_effect() => result,
            }
        };
        if cancellation.is_cancelled() {
            return process_lashlang_cancelled("lashlang process was cancelled").into();
        }
        match execution {
            Ok(lashlang::VmRunOutcome::Complete(output)) => {
                vm.flush_profile(compiled.as_ref(), env);
                return process_lashlang_execution_result(Ok(output)).into();
            }
            Err(err) => {
                vm.flush_profile(compiled.as_ref(), env);
                return process_lashlang_execution_result(Err(err)).into();
            }
            Ok(lashlang::VmRunOutcome::EffectCompleted) => {
                progress.effects_executed += 1;
                let Some(reason) = controller.wants_segment_boundary(&progress) else {
                    continue;
                };
                match vm.suspend() {
                    Ok(continuation) => {
                        let segment_state = LashlangSegmentState {
                            version: LASHLANG_SEGMENT_STATE_VERSION,
                            vm: continuation,
                            ordinals: host.ordinals.snapshot().await,
                            started_process_ids: host.ctx.started_process_ids(),
                            child_max_attempts: host.child_max_attempts,
                            incorporation_ledger: host.ctx.incorporation_ledger_snapshot(),
                            effect_omissions: host.effect_summary.omissions(),
                        };
                        match serde_json::to_vec(&segment_state) {
                            Ok(engine_state) => {
                                return lash_core::ProcessRunOutcome::SegmentBoundary(
                                    lash_core::SegmentHandover {
                                        reason,
                                        program_hash: program_hash.clone(),
                                        engine_state,
                                    },
                                );
                            }
                            Err(err) => {
                                record_segment_boundary_decline(
                                    &err,
                                    "lashlang segment continuation was not serializable; continuing",
                                );
                            }
                        }
                    }
                    Err(err) => {
                        record_segment_boundary_decline(
                            &err,
                            "lashlang segment boundary declined at non-capturable point",
                        );
                    }
                }
            }
        }
    }
}

struct LashlangProcessHost<'run> {
    ctx: lash_core::RuntimeExecutionContext<'run>,
    host_environment: lashlang::LashlangHostEnvironment,
    artifact_store: Arc<dyn lashlang::LashlangArtifactStore>,
    processes: lash_core::facade_support::ProcessEngineProcessContext,
    process_id: ProcessId,
    /// The one derivation of leaf, child and group identities this tier mints,
    /// shared with the RLM cell bridge. The authority is this process
    /// incarnation, never a segment: a body that hands over keeps minting from
    /// the scope its first segment used.
    identities: crate::LashlangHostIdentities,
    lashlang_execution_trace: LashlangProcessExecutionTrace,
    /// The replay ordinals this segment is consuming: restored from the
    /// handover that resumed the run (or zeroed for a first segment) and
    /// snapshotted into the next boundary's envelope.
    ordinals: ReplayOrdinals,
    /// Attempt bound stamped onto every child this run starts, resolved once
    /// at the run's first segment and replayed from segment state afterwards.
    child_max_attempts: std::num::NonZeroU32,
    /// This run's cancellation scope, read by the VM's cooperative cancellation
    /// probe so a cancelled process terminates as an uncatchable host terminal
    /// instead of running to completion inside a guest handler. It carries the
    /// engine's cancellation and the cancellations this run observes for itself,
    /// which is where a cancelled tool call lands.
    cancellation: crate::ExecutionCancellation,
    /// The durable effect summary this run writes at result incorporation.
    effect_summary: EffectSummaryWriter,
}

#[async_trait::async_trait]
trait SignalWaitProcesses: Send + Sync {
    async fn current_wait(&self) -> Result<Option<lash_core::WaitState>, lash_core::PluginError>;

    async fn event_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core::ProcessEventPageToken>,
    ) -> Result<
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_core::PluginError,
    >;

    async fn set_wait(&self, wait: lash_core::WaitState) -> Result<(), lash_core::PluginError>;
}

#[async_trait::async_trait]
impl SignalWaitProcesses for lash_core::facade_support::ProcessEngineProcessContext {
    async fn current_wait(&self) -> Result<Option<lash_core::WaitState>, lash_core::PluginError> {
        Ok(self.record().await?.and_then(|record| record.wait))
    }

    async fn event_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core::ProcessEventPageToken>,
    ) -> Result<
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_core::PluginError,
    > {
        self.event_page(limit, lash_core::ProcessEventQueryMode::Full, continuation)
            .await
    }

    async fn set_wait(&self, wait: lash_core::WaitState) -> Result<(), lash_core::PluginError> {
        self.set_wait(wait).await.map(|_| ())
    }
}

enum SignalWaitSetupError {
    Read(lash_core::PluginError),
    Set(lash_core::PluginError),
}

async fn establish_signal_wait(
    processes: &dyn SignalWaitProcesses,
    process_id: &ProcessId,
    name: String,
    event_type: String,
    key: String,
    ordinal: u64,
) -> Result<(), SignalWaitSetupError> {
    let since_ms = wait_since_ms(processes, process_id, &key)
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
    processes
        .set_wait(wait)
        .await
        .map_err(SignalWaitSetupError::Set)?;
    Ok(())
}

async fn wait_since_ms(
    processes: &dyn SignalWaitProcesses,
    process_id: &ProcessId,
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
    let mut continuation = None;
    let mut matched_since_ms = None;
    loop {
        let outcome = processes.event_page(limit, continuation).await?;
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
            lash_core::ProcessEventReadOutcome::NoLongerRetained(
                lash_core::ProcessEventHistoryRetention::Retired {
                    requested_incarnation,
                    current_incarnation,
                },
            ) => {
                return Err(lash_core::PluginError::ProcessIncarnationSuperseded {
                    process_id: process_id.clone(),
                    requested_incarnation,
                    current_incarnation,
                });
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
        continuation = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { continuation } => Some(continuation),
        };
    }
    Ok(matched_since_ms.unwrap_or_else(lash_core::facade_support::current_epoch_ms))
}

type ProcessHostAbilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<lashlang::AbilityResult, ExecutionHostError>> + Send + 'a>>;

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

    /// This tier refuses a leaf with no call site (see
    /// [`prepare_resource_invocation`]), so the position is always a site here.
    fn resource_tool_call_id(
        &self,
        host_operation: &str,
        call_site: &lashlang::LashlangExecutionCallSite,
        batch_index: Option<usize>,
    ) -> String {
        let call_id = match batch_index {
            Some(batch_index) => self
                .identities
                .child(host_operation, call_site, batch_index),
            None => self.identities.leaf(host_operation, call_site),
        };
        self.lashlang_execution_trace
            .record_resource_call(call_site, &call_id);
        call_id
    }

    async fn resource_operation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        if crate::is_typescript_runtime_receiver(&receiver) {
            let call_site = call_site.as_ref().ok_or_else(|| {
                ExecutionHostError::new("TypeScript runtime operation is missing its call site")
            })?;
            return self
                .typescript_runtime_value(&receiver, &operation, &args, call_site, None)
                .await;
        }
        let invocation =
            self.prepare_resource_invocation(operation, receiver, args, call_site, None)?;
        let invocation = match invocation {
            PreparedResourceInvocation::Trigger {
                operation,
                payload,
                effect_id,
                host_operation,
                call_site,
            } => {
                return self
                    .trigger_operation(operation, payload, effect_id, &host_operation, &call_site)
                    .await;
            }
            PreparedResourceInvocation::Tool {
                invocation,
                host_operation,
                call_site,
            } => (invocation, host_operation, call_site),
        };
        let (invocation, host_operation, call_site) = invocation;
        let lash_core::facade_support::ToolInvocation {
            id,
            tool_id,
            args,
            execution_grant: _,
            child_execution_trace_hook,
            issuing_language_node_id,
        } = invocation;
        let tool_ctx = issuing_language_node_id
            .map(|node_id| self.ctx.clone().with_issuing_language_node_id(node_id))
            .unwrap_or_else(|| self.ctx.clone());
        let replay_key = id.clone();
        let reply = if let Some(call_site) = child_execution_trace_hook {
            tool_ctx
                .call_tool_by_id_with_child_execution_trace_hook(id, tool_id, args, 0, call_site)
                .await
        } else {
            Box::pin(tool_ctx.call_tool_by_id(id, tool_id, args, 0)).await
        };
        self.record_tool_reply(&call_site, &host_operation, &replay_key, &reply)
            .await;
        protocol_tool_reply_to_lashlang_value(reply, &replay_key, &self.cancellation)
    }

    async fn await_handle(
        &self,
        handle: lashlang::Value,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let replay_key = uuid::Uuid::new_v4().to_string();
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.await_handle");
            self.ctx
                .await_tool_handle(replay_key.clone(), lashlang_value_to_json(&handle)?)
                .await
        };
        protocol_tool_reply_to_lashlang_value(reply, &replay_key, &self.cancellation)
    }

    async fn process_event(&self, event: lashlang::ProcessEvent) -> Result<(), ExecutionHostError> {
        let event_type = match event.kind {
            lashlang::ProcessEventKind::Yield => "process.yield",
            lashlang::ProcessEventKind::Wake => "process.wake",
        };
        let ordinal = self.ordinals.event_sequence.fetch_add(1, Ordering::Relaxed);
        self.ctx
            .append_process_event(
                lash_core::ProcessEventAppendRequest::new(
                    event_type,
                    process_event_payload(&event.value)?,
                )
                .with_replay_key(format!("process:{}:event:{ordinal}", self.process_id)),
            )
            .await
            .map_err(|error| LashlangHostError::AppendProcessEvent {
                message: error.to_string(),
            })?;
        Ok(())
    }

    async fn sleep(&self, sleep: lashlang::Sleep) -> Result<lashlang::Value, ExecutionHostError> {
        let call_site = sleep.call_site;
        let sleep = process_sleep(sleep.kind, &sleep.value)?;
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
        let sequence = self.ordinals.sleep_sequence.fetch_add(1, Ordering::Relaxed);
        let scope = format!("process:{}", self.process_id);
        let slept = self.ctx.sleep_process(&scope, sequence, sleep).await;
        // The effect host journals the wake verdict, so a success or a
        // recorded cancellation is replay-stable; any other error has no
        // recorded outcome to summarise.
        let outcome_class = match &slept {
            Ok(()) => Some(lash_core::ProcessEffectOutcomeClass::Success),
            Err(error)
                if error.code == lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled =>
            {
                Some(lash_core::ProcessEffectOutcomeClass::Cancelled)
            }
            Err(_) => None,
        };
        if let (Some(call_site), Some(outcome_class)) = (&call_site, outcome_class) {
            self.record_effect_outcome(
                call_site,
                lash_core::runtime::causal::PROCESS_SLEEP_OPERATION,
                outcome_class,
                None,
                &self.ctx.process_sleep_replay_key(&scope, sequence),
            )
            .await;
        }
        slept.map_err(|error| LashlangHostError::SleepProcess {
            message: error.to_string(),
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
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let event_type =
            lash_core::facade_support::process_signal_event_type(&name).map_err(|error| {
                LashlangHostError::ValidateSignalName {
                    message: error.to_string(),
                }
            })?;
        let event_ordinal = {
            let mut wait_ordinals = self.ordinals.signal_wait_ordinals.lock().await;
            let ordinal = wait_ordinals.entry(name.clone()).or_insert(0);
            *ordinal += 1;
            *ordinal
        };
        let key = lash_core::facade_support::process_signal_wait_key(
            &self.process_id,
            &name,
            event_ordinal,
        );
        establish_signal_wait(
            &self.processes,
            &self.process_id,
            name.clone(),
            event_type,
            key.clone(),
            event_ordinal,
        )
        .await
        .map_err(|error| match error {
            SignalWaitSetupError::Read(error) => LashlangHostError::ReadSignalWait {
                message: error.to_string(),
            },
            SignalWaitSetupError::Set(error) => LashlangHostError::SetSignalWait {
                message: error.to_string(),
            },
        })?;
        if let Some(call_site) = &call_site {
            self.lashlang_execution_trace.emit_waiting(
                call_site,
                TraceNodeAwaited::Signal {
                    name: name.clone(),
                    key,
                },
            );
        }
        let payload = self
            .ctx
            .await_process_signal_event(&self.process_id, &name, event_ordinal)
            .await
            .map_err(|error| LashlangHostError::AwaitSignal {
                message: error.to_string(),
            })?;
        self.processes
            .clear_wait()
            .await
            .map_err(|error| LashlangHostError::ClearSignalWait {
                message: error.to_string(),
            })?;
        if let Some(call_site) = &call_site
            && !self.cancellation.is_cancelled()
        {
            self.lashlang_execution_trace
                .emit_resumed(call_site, TraceNodeWaitResolution::Resumed);
        }
        Ok(lashlang::from_json(payload))
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
                .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::ResourceOperationBatch(batch) => Box::pin(async move {
                Ok(lashlang::AbilityResult::ResourceOperationBatch(
                    self.resource_operation_batch(batch).await,
                ))
            }),
            lashlang::AbilityOp::Await(handle) => Box::pin(async move {
                self.await_handle(handle)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::ProcessEvent(event) => Box::pin(async move {
                self.process_event(event).await?;
                Ok(lashlang::AbilityResult::Unit)
            }),
            lashlang::AbilityOp::Sleep(sleep) => {
                Box::pin(async move { self.sleep(sleep).await.map(lashlang::AbilityResult::Value) })
            }
            lashlang::AbilityOp::WaitSignal { name, call_site } => Box::pin(async move {
                self.wait_signal(name, call_site)
                    .await
                    .map(lashlang::AbilityResult::Value)
            }),
            lashlang::AbilityOp::Print(_) => {
                Box::pin(async { Err(LashlangHostError::PrintUnavailable.into()) })
            }
            lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(lashlang::AbilityResult::Value(value)) })
            }
        }
    }
}

impl lashlang::ExecutionHost for LashlangProcessHost<'_> {
    fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> impl Future<Output = Result<lashlang::AbilityResult, ExecutionHostError>> + Send {
        self.perform_selected_ability(op)
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
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
    sink: Option<Arc<dyn TraceSink>>,
    base_context: TraceContext,
    session_id: Option<SessionId>,
    process_id: ProcessId,
    source_identity: String,
    module_ref: lashlang::ModuleRef,
    process_ref: lashlang::ProcessRef,
    process_name: String,
    attempt: u32,
    incarnation: lash_core::ProcessIncarnation,
    restate_invocation_id: Option<String>,
    resource_call_ids: Arc<std::sync::Mutex<BTreeMap<(String, u64), String>>>,
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
    incarnation: lash_core::ProcessIncarnation,
    restate_invocation_id: Option<String>,
}

#[path = "process/execution_trace.rs"]
mod execution_trace;

fn process_trace_session_id(originator: &lash_core::ProcessOriginator) -> Option<SessionId> {
    match originator {
        lash_core::ProcessOriginator::Session { session_id, .. } => Some(session_id.clone()),
        lash_core::ProcessOriginator::Host { .. } => None,
    }
}

fn process_lashlang_execution_result(
    result: Result<lashlang::ExecutionOutcome, lashlang::RuntimeError>,
) -> lash_core::ProcessAwaitOutput {
    match result {
        Ok(lashlang::ExecutionOutcome::Finished(value)) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                lashlang_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ))
        }
        Ok(lashlang::ExecutionOutcome::Failed(value)) => process_lashlang_failure(
            LashlangProcessFailureCode::ProcessFailed,
            value.to_string(),
            Some(
                lashlang_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ),
        ),
        Ok(lashlang::ExecutionOutcome::Continued) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
        }
        Err(err) => {
            let exhausted = err.is_execution_bound_exhausted();
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst) || !exhausted,
                "confidence durable process exhausted a required Lashlang bound: {err}"
            );
            process_lashlang_failure(
                if exhausted {
                    LashlangProcessFailureCode::ProcessExecutionBoundExhausted
                } else {
                    LashlangProcessFailureCode::ProcessRuntimeError
                },
                err.to_string(),
                None,
            )
        }
    }
}

fn process_lashlang_failure(
    code: LashlangProcessFailureCode,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> lash_core::ProcessAwaitOutput {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        code.as_str(),
        message,
    );
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
    lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(failure))
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
    trace_lashlang_process_map_snapshot, trace_lashlang_source_identity,
};

#[path = "process/resource_invocation.rs"]
mod resource_invocation;
use resource_invocation::PreparedResourceInvocation;

#[cfg(test)]
#[path = "process/segment_trace_tests.rs"]
mod segment_trace_tests;
#[cfg(test)]
#[path = "process/signal_wait_tests.rs"]
mod signal_wait_tests;
