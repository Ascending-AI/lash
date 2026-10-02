/// version_surface = "coexist"
/// version_guard(items(LASH_LASHLANG_PROGRAM_DOMAIN_VERSION, lashlang_program_hash))
const LASH_LASHLANG_PROGRAM_DOMAIN_VERSION: &str = "lash-lashlang-program/v3";

mod execution_result;
mod segment_state;
use execution_result::{
    process_lashlang_execution_result, process_lashlang_failure, process_worker_failure,
};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use segment_state::capture_segment;
mod definition_holds;
use definition_holds::hold_segment_definitions;
use lash_sansio::{ProcessId, SessionId};
use std::collections::BTreeMap;
use std::future::Future;
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
/// meaning. The remaining ordinals move into one `ReplayOrdinalsState`
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
/// v17 carries the effect groups the process still holds — each group's key,
/// child count and consumed cursor — so a successor segment reattaches losers
/// that are still running instead of declining the boundary (ADR 0099 §8, §9),
/// and embeds VM continuation v18.
/// v18 (FIG-3571) embeds VM continuation v19 over the carrier IR's node ids. A
/// v17 segment parked before the cutover is refused before continuation
/// restore; it is never re-driven under the new node ids.
/// v19 (FIG-3586) carries the run's issue-ordinal state — the ordinal the
/// next command takes and the running digest of the commands it wrote — in
/// place of the per-kind sleep sequence, and embeds VM continuation state
/// whose aggregates no longer count occurrences. A segment parked by v18
/// resumes commands under keys a v19 run never mints, so it is refused.
/// v20 (FIG-3655) embeds VM continuation v22, whose closures carry their own
/// `name`/`length` metadata. A v19 segment holds continuations in the v21
/// shape, so it is refused rather than decoded.
/// v21 (FIG-3701) embeds VM continuation v24, whose heap may hold a built-in
/// method value (`'x'.includes`). A v20 segment holds v23 continuations, so it
/// is refused rather than decoded.
/// v22 (FIG-3707) embeds VM continuation v25, whose heap may hold a binding
/// cell. A v21 segment holds v24 continuations, so it is refused rather than
/// decoded.
/// v23 (FIG-3571) carries the run's pending effect-summary occurrences, which
/// the successor commits at its first boundary. A v22 segment committed each
/// occurrence as it was recorded and carries none, so it is refused rather
/// than decoded.
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store.
///
/// version_guard(
///     roots(LashlangSegmentState),
///     items(path = "crates/lashlang/src/workflow_graph.rs", workflow_node_id),
///     items(
///         path = "crates/lashlang/src/workflow_graph/execution_sites.rs",
///         path = "crates/lashlang/src/workflow_graph/ownership.rs",
///         path = "crates/lashlang/src/ast_roles.rs", path = "crates/lashlang/src/tracking.rs",
///         from_indices, indices, path_for_ast, for_main, for_process, ownership_map,
///         into_ownership_map, workflow_projection, statement_list, push_statement_list,
///         is_statement_list, collect_body, collect_statement, statement_value, map_node_subtree,
///         check_shape, process_wrapper_run_path, execution_sites, collect_execution_sites,
///         push_execution_site_descriptor, collect_child_execution_sites, workflow_owner,
///         node_site, branch_site, branch_edge_id,
///     ),
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/process/engine.rs",
///         cover(SegmentHandover, PersistedSegmentHandover),
///     ),
/// )
pub const LASHLANG_SEGMENT_STATE_VERSION: u32 = 23;

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
    /// The run's issue-ordinal state (FIG-3586): every command's journal key.
    commands: crate::LashlangRunOrdinals,
    /// Process-event sequence: the idempotency key of each event append,
    /// which observers address.
    event_sequence: u64,
    /// Per-name signal-wait ordinals: the wait keys outside signallers
    /// address, so they stay named.
    signal_wait_ordinals: BTreeMap<String, u64>,
}

/// The live counterpart of [`ReplayOrdinalsState`]: the ordinals the running
/// segment is consuming, held as the counters the host mutates in place. The
/// command ordinals live in the run itself.
struct ReplayOrdinals {
    event_sequence: AtomicU64,
    signal_wait_ordinals: std::sync::Mutex<BTreeMap<String, u64>>,
}

impl ReplayOrdinals {
    fn restore(state: Option<&LashlangSegmentState>) -> Self {
        let ordinals = state.map(|state| &state.ordinals);
        Self {
            event_sequence: AtomicU64::new(ordinals.map_or(0, |o| o.event_sequence)),
            signal_wait_ordinals: std::sync::Mutex::new(
                ordinals.map_or_else(BTreeMap::new, |o| o.signal_wait_ordinals.clone()),
            ),
        }
    }

    /// The command ordinals a resumed segment continues from, or a fresh
    /// run's.
    fn restore_commands(state: Option<&LashlangSegmentState>) -> crate::LashlangRunOrdinals {
        state.map_or_else(crate::LashlangRunOrdinals::start, |state| {
            state.ordinals.commands.clone()
        })
    }

    fn snapshot(&self, run: &crate::LashlangReplayRun) -> ReplayOrdinalsState {
        ReplayOrdinalsState {
            commands: run.ordinals(),
            event_sequence: self.event_sequence.load(Ordering::Relaxed),
            signal_wait_ordinals: self.signal_wait_ordinals.lock_recover().clone(),
        }
    }
}

/// The most continuation bytes a parked segment may carry: the preset the
/// measurement lane finalises.
const MAX_SEGMENT_CONTINUATION_BYTES: u64 = 64 * 1024 * 1024;

/// The segment envelope a boundary hands to the next segment.
///
/// It is assembled by the parent: `vm` is the worker's continuation, held as
/// opaque bytes the parent checks structurally and never decodes (ADR 0123),
/// and every other field is a ledger the parent owns — the ordinals, the
/// started children, incorporation, the pending summary and the groups. The
/// worker contributes the VM bytes and nothing else.
#[derive(serde::Serialize, serde::Deserialize)]
struct LashlangSegmentState {
    version: u32,
    vm: lash_vm_protocol::OpaqueVmState,
    #[serde(flatten)]
    ordinals: ReplayOrdinalsState,
    started_process_ids: Vec<ProcessId>,
    /// The once-only settlement incorporation ledger (FIG-3411): a successor
    /// segment incorporates against the same set so a redrive cannot
    /// re-apply a settlement.
    incorporation_ledger: lash_core::session::IncorporationLedger,
    /// Effect occurrences within the durable summary's per-node cap that no
    /// boundary has committed yet (FIG-3571). The successor segment commits
    /// them with its first boundary write; re-committing one a crashed
    /// successor already wrote is a replay-key no-op. Bounded by construction:
    /// at most [`lash_core::PROCESS_EFFECT_OCCURRENCE_CAP`] per execution site
    /// of the compiled program.
    pending_summary: Vec<lash_core::ProcessEffectOccurrence>,
    /// Effect occurrences past the durable summary's per-node cap, counted by
    /// outcome class (FIG-3464). A successor segment keeps counting from here
    /// and the run's terminal omission record carries the total.
    effect_omissions: BTreeMap<String, lash_core::ProcessEffectOmittedCounts>,
    /// The effect groups this process still holds after an aggregate stopped
    /// consuming early (ADR 0099 §8, §9): each group's key, child count and
    /// consumed cursor. A boundary is never declined because a loser is
    /// unsettled; the successor segment reattaches these cursors and the
    /// process terminal closes them.
    outstanding_groups: Vec<lash_core::EffectGroupHandle>,
    /// The tool calls each held group counts against the session's
    /// `max_tool_calls` (FIG-4546), by group key. The successor segment is
    /// the same process, so it holds the same calls: it reuses these
    /// reservations rather than counting the groups again or not at all.
    held_tool_calls: BTreeMap<String, usize>,
    /// The worker accounting the body carries across this boundary
    /// (ADR 0123). Absent from a handover written before it existed: that
    /// successor reserves boundary 0's successor with fresh totals.
    #[serde(default)]
    worker_recovery: WorkerRecoveryLedger,
}

#[cfg(test)]
pub(crate) fn decode_lashlang_segment_state_for_tests(data: &[u8]) -> Result<(), String> {
    decode_lashlang_segment_state(data)
        .map(|_| ())
        .map_err(|error| error.to_string())
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
            lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
        })?;
    engine.workers = recovery.service().clone();
    let result = Box::pin(run_lashlang_process_scoped(
        engine, context, payload, handover, ledger,
    ))
    .await;
    recovery.settle().await.map_err(|error| {
        lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
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
        engine.execution_sink.clone(),
        engine.trace_context.clone(),
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
    // The drive's recorded cancellation fact (FIG-3673): advanced only by what
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
        ctx.restore_started_process_ids(&segment_state.started_process_ids);
        ctx.restore_incorporation_ledger(segment_state.incorporation_ledger.clone());
        ctx.restore_outstanding_groups(
            std::mem::take(&mut segment_state.outstanding_groups),
            &segment_state.held_tool_calls,
        );
    }
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
        effect_summary: segment_state
            .as_ref()
            .map_or_else(EffectSummaryWriter::default, |state| {
                EffectSummaryWriter::restore(
                    state.pending_summary.clone(),
                    state.effect_omissions.clone(),
                )
            }),
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
            *prelude = host
                .effect_summary
                .terminal_prelude(host.identities.effect_omissions(), host.ctx.fleet_format());
            adopt_held_attachments(&host, output).await?;
        }
    }
    // A process terminal is the process opener's end (ADR 0099 §7): every
    // effect group it still holds is closed and finalized, and its losers'
    // settled facts incorporated, before the terminal is handed back to be
    // committed. A segment boundary is not an end — the successor reattaches
    // the cursors the handover carried. A failed close leaves `closing`
    // recorded and surfaces as infrastructure, so the run is retried rather
    // than committing a terminal whose accounting was never incorporated.
    if output.is_terminal() && !refused && host_failure.is_none() {
        let _phase = host.ctx.named_phase("rlm_process.close_groups");
        host.ctx.close_opener_groups().await.map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::RuntimeEffectController(
                error,
            ))
        })?;
    }
    drop(host);
    {
        let _phase =
            lash_core::runtime::RuntimeNamedPhase::begin(phase_probe, "rlm_process.shutdown");
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
        lash_core::ProcessInfraError::new(lash_core::PluginError::Session(message))
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
            lash_core::ProcessRunOutcome::SegmentBoundary(
                capture_segment(
                    checkpoint.vm,
                    host,
                    reason
                        .lock_recover()
                        .take()
                        .unwrap_or(lash_core::BoundaryReason::HandOver),
                    &program_hash,
                )
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
        Ok(self.record().await?.and_then(|record| record.wait))
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
    ) -> Result<lashlang::Value, ExecutionHostError> {
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
            return result;
        }
        let call_id = self.identities.call_id(command.ordinal);
        let prepared = match self.prepare_resource_invocation(
            operation,
            receiver,
            args,
            call_site,
            call_id,
            command.key.as_str().to_string(),
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
                effect_id,
                host_operation,
                call_site,
            } => {
                let in_flight = commands.enter(command, crate::CommandShape::Value).await?;
                let result = self
                    .trigger_operation(
                        &in_flight.ctx,
                        operation,
                        payload,
                        effect_id,
                        &host_operation,
                        call_site.as_ref(),
                    )
                    .await;
                commands.finish(&in_flight)?;
                result
            }
            PreparedResourceInvocation::Tool {
                invocation,
                host_operation,
                call_site,
            } => {
                // The call's journal rows live under its command key; that is
                // the replay key its summary and its failures name.
                let replay_key = command.key.as_str().to_string();
                let in_flight = commands
                    .enter(command, crate::CommandShape::ToolCall)
                    .await?;
                let reply = Box::pin(
                    in_flight
                        .ctx
                        .call_command_tool(&in_flight.command.key, invocation),
                )
                .await;
                commands.finish(&in_flight)?;
                if let Some(call_site) = &call_site {
                    self.record_tool_reply(call_site, &host_operation, &replay_key, &reply);
                }
                protocol_tool_reply_to_lashlang_value(reply, &replay_key, &self.cancellation)
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
                // timer: the drive's fact advances here, at the same point on
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
                .map(lashlang::AbilityOutcome::Value)
            }),
            lashlang::AbilityOp::ResourceOperationBatch(batch) => Box::pin(async move {
                self.resource_operation_batch(batch)
                    .await
                    .map(lashlang::AbilityOutcome::ResourceOperationBatch)
            }),
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
    sink: Option<Arc<dyn TraceSink>>,
    base_context: TraceContext,
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
mod segment_trace_tests;
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
