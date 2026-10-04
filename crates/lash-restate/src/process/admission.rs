//! Segment admission: the resume fence every Restate process segment passes
//! before its first effect (FIG-3588).
//!
//! Lash never restarts started work from scratch. A Restate workflow's
//! invocation id is a function of its workflow key, so an invocation id cannot
//! tell a retry of the invocation that started a segment (whose journal
//! replays its effects) from a fresh invocation of the same key after that
//! journal was lost (which would run them again). The discriminator is a
//! durable start marker, bound to a nonce the admitting execution journaled.
//!
//! The ordering is the contract, and each step is its own journaled command:
//!
//! 1. **Verdict** (`lash.segment.admit`, read-only). Read the segment's start
//!    marker — segment 0's is the process's `first_started`, a later segment's
//!    is the [`SegmentStartMarker`](lash_core::SegmentStartMarker) on its
//!    retained handover. Present: the segment started under a journal this
//!    invocation cannot read, so the process ends `Abandoned` with
//!    `ResumeRefused { SubstrateLost }`. Absent: admit, and draw a fresh nonce
//!    from OS randomness *inside* the step, so the nonce is journaled with the
//!    verdict. A segment whose successor handover already exists completed; it
//!    is superseded, never refused. A later segment whose own handover is
//!    missing is refused terminally. The verdict also journals the segment's
//!    inputs (FIG-3673): the digest of the handover it resumes from and the
//!    boundary policy it cuts under, so no live read or host setting decides
//!    the journal's shape on a redrive.
//! 2. **Start** (`lash.segment.start`). Record the marker with that nonce,
//!    set-if-absent. The recorded nonce equals ours: this execution's marker,
//!    written now or by this execution's own earlier try, and the step journals
//!    the process it started, so nothing after it reads the record
//!    live. A different nonce: an execution this one does not continue started
//!    the segment, so the process ends `SubstrateLost`.
//! 3. **Effects**, only with the [`SegmentStarted`] proof step 2 returns. The
//!    proof has no public constructor, and the process segment's controller
//!    and the runner both require it, so no effect can precede the committed
//!    marker.
//!
//! Drawing the nonce and writing the marker must stay two journaled steps: a
//! retry of a step whose completion was never journaled re-runs its closure,
//! so one step that both drew and wrote would draw a second nonce on retry and
//! refuse its own marker. The nonce never comes from the Restate context's RNG
//! or the invocation id, both of which repeat after a purge.

use lash_core::{
    PluginError, ProcessExecutionWriteAuthority, ProcessId, ProcessRecord, ProcessRegistry,
    ProcessSegmentKey, ProcessStarted, SegmentStartMarker,
};
use restate_sdk::context::{ContextSideEffects, RunFuture, WorkflowContext};
use restate_sdk::errors::{HandlerError, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The generation of the Restate process handler's journaled commands.
///
/// This version owns the commands every `LashProcessWorkflow/run` invocation
/// journals around its runner: the generation sentinel first, the successor
/// window a stable-lane successor from another build passes, the admission
/// verdict and the start step above, the segment's recorded cancel races and
/// peeks, and the terminal, boundary and handover steps after it (FIG-3673,
/// FIG-3795). Any change to those commands, or to what they key on, bumps it
/// (paused pre-1.0, FIG-3660). It is a drain format, so it moves the build's
/// generation `G`: a journal of another generation is refused by the
/// generation sentinel on replay, and a new invocation is routed by the
/// sender generation its input carries
/// ([`RestateProcessWorkflowInput::sender_generation`](super::RestateProcessWorkflowInput::sender_generation)).
/// Generation 4 (FIG-3607) names the process by its minted id alone: the
/// input carries the id beside its registration, which no longer names one,
/// the start step journals the id it started, and the requests a caller sends
/// into a running workflow (complete, await, cancel, attach) are stamped with
/// this generation and refused by it before their shape is decoded. An
/// unstamped request is generation 1. Generation 4 changed in place under the
/// pre-1.0 version freeze (FIG-3846): a registration records its lifetime,
/// ancestry and session capability where it carried a parent policy
/// (FIG-3607), and the input carries its sender's drain generation in place
/// of this version, behind the generation sentinel (FIG-3795).
/// Generation 5 (FIG-4849) pins one process segment's effect journal instead
/// of recording every effect's begin and end at the scope index.
///
/// version_guard(
///     roots(AdmissionVerdict, StartOutcome),
///     roots(
///         path = "crates/lash-restate/src/process/mod.rs", RestateProcessWorkflowInput,
///         RestateProcessWorkflowPayload, RestateProcessCancelRequest,
///         RestateProcessCompleteRequest, RestateProcessAwaitRequest,
///     ),
///     roots(path = "crates/lash-restate/src/process_attach.rs", RestateProcessAttachRequest),
///     items(ADMIT_STEP, START_STEP, stamped_journal_version, decode_stamped_request),
///     items(path = "crates/lash-restate/src/controller/scope_recording.rs", execute_effect),
///     roots(path = "crates/lash-restate/src/durable_wait/messages.rs", RestateDurableWaitProcessJournalRequest),
///     items(path = "crates/lash-restate/src/process/workflow.rs", run),
///     items(path = "crates/lash-restate/src/process/workflow/scope_journal.rs", register, release),
///     items(path = "crates/lash-restate/src/durable_wait/scope_retirement.rs", register_process_journal, release_process_journal),
///     file(path = "crates/lash-restate/src/process/stamped_requests.rs"),
/// )
/// version_surface = "drain"
/// format_manifest = "engine:restate.process_journal"
pub const RESTATE_PROCESS_JOURNAL_VERSION: u32 = 5;

/// The manual epoch of the journal-bearing handlers' logic, hashed into the
/// build's drain generation beside the drain-format versions (FIG-3795).
///
/// A handler change can move what a journal means without moving any format
/// version: the order of journaled steps, a step's name, what a recorded
/// verdict implies. Hashing the drain counters alone cannot see such a
/// change, so one lands by bumping this epoch, which changes the build's
/// generation and keeps the old journal replaying only under its own build.
/// The bump guard pins the handler prefix steps below.
/// Epoch 3 (FIG-4914) separates FIG-4857's tagged plugin-transition bases
/// and added process/command transition steps from the preceding journals.
/// Epoch 4 (FIG-4855) records presentation callback selection before the
/// presentation effect, separating that added step from preceding journals.
/// Epoch 5 retains scalar tool requests before execution (FIG-4830).
/// Epoch 6 (FIG-4878) separates the recorded plugin-callback steps and the
/// state resolutions effect outcomes carry from the preceding journals.
/// Epoch 7 (FIG-4848) separates shifts that stop on an empty commit receipt
/// from shifts that journal another boundary and admission.
/// Epoch 8 (FIG-4848) records the admitted head verdict in the admission
/// instead of journaling a separate inspection.
/// Epoch 9 (FIG-4879) records independent owned attempts and their retry/
/// selection schedule, with state resolutions in the selected decision.
/// Epoch 10 (FIG-4881) permits program progress between selected results and
/// local quiescence before capturing a physical cut.
/// Epoch 11 (FIG-4848) retains the prepared context and pressure decisions
/// in the environment prelude, referencing the early configuration record.
/// Epoch 12 (FIG-4740) removes runtime deadline steps and timers, and arms
/// Deferred sources before X with Run-owned subscriptions and sealed decisions.
/// Epoch 13 (FIG-4920) replays command and operation plugin transitions before
/// their command-lane reads after session deletion.
/// Epoch 14 (FIG-4848) keys each turn invocation by immutable shift intent
/// and ordinal, and records selection in that invocation before execution.
/// Epoch 15 (FIG-4882) records aggregate membership and timer wakes, separates
/// consumer possession from protected drain, and records logical Closing.
/// Epoch 16 (FIG-4887) publishes process terminals to short subscriptions
/// instead of attach workflows.
/// Epoch 17 (FIG-4922) removes result-check state-only steps; a route without
/// a recorded decision refuses its commands before publication.
/// Epoch 18 (FIG-4923) carries full plugin-state receipt identities and
/// checkpointed publication ownership, fencing predecessor segments.
/// Epoch 19 (FIG-4926) stores namespace snapshot references in admission and
/// accepts presentation fallback before publishing the attempt stream.
/// Epoch 20 (FIG-4921) records callback session contributions and publishes
/// catalog membership and graph appends only after their step acknowledges.
/// Epoch 21 (FIG-4890) records the admitted environment and adopts complete
/// Run receipts under the successor segment before any new publication.
///
/// version_guard(
///     roots(AdmissionVerdict, StartOutcome),
///     items(ADMIT_STEP, START_STEP),
///     items(
///         path = "crates/lash-restate/src/process/workflow.rs", COMPLETE_STEP, BOUNDARY_STEP,
///         HANDOVER_STEP, CANCEL_FORWARD_STEP, CANCEL_RECORD_STEP, CANCEL_ROUTE_STEP,
///         CANCEL_CHILD_TURN_STEP, RETIRE_STEP,
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "drain"
/// format_outside_manifest = "not a durable format version: it is an input to the build generation (formats::composed_generation), not a row in the durable-format manifest"
pub const JOURNAL_LOGIC_EPOCH: u32 = 21;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the epoch, so its `G` and
/// its generation lanes differ from N's.
#[cfg(feature = "synthetic-next")]
/// version_surface = "drain"
/// format_outside_manifest = "not a durable format version: it is an input to the build generation (formats::composed_generation), not a row in the durable-format manifest"
pub const JOURNAL_LOGIC_EPOCH: u32 = 22;

/// The journal name of the verdict step.
const ADMIT_STEP: &str = "lash.segment.admit";
/// The journal name of the start step.
const START_STEP: &str = "lash.segment.start";

/// The generation a request that carries no stamp was written by.
fn unstamped_journal_version() -> u32 {
    1
}

/// The generation a process request was written by, read before its shape
/// is decoded: a request of another generation is refused by generation,
/// never by a shape error its retired fields would raise.
fn stamped_journal_version(payload: &serde_json::Value) -> u32 {
    payload
        .get("journal_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or_else(unstamped_journal_version)
}

/// Decodes a process request sent into a running workflow, refusing a request
/// of another journal generation before decoding its shape.
///
/// # Errors
///
/// A terminal error naming the retired generation, or naming why a request of
/// this generation does not decode.
pub(crate) fn decode_stamped_request<T: serde::de::DeserializeOwned>(
    request_kind: &str,
    payload: serde_json::Value,
) -> Result<T, TerminalError> {
    let version = stamped_journal_version(&payload);
    if version != RESTATE_PROCESS_JOURNAL_VERSION {
        return Err(TerminalError::new(format!(
            "{request_kind} carries restate-process-journal-v{version}; this handler serves generation {RESTATE_PROCESS_JOURNAL_VERSION}"
        )));
    }
    serde_json::from_value(payload).map_err(|error| {
        TerminalError::new(format!(
            "{request_kind} of generation {RESTATE_PROCESS_JOURNAL_VERSION} does not decode: {error}"
        ))
    })
}

/// Proof that this execution's segment start marker committed.
///
/// Minted only by [`admit_segment`], after its start step journaled. The
/// process segment's effect controller
/// ([`RestateRuntimeEffectController::process_segment_controller`](crate::RestateRuntimeEffectController::process_segment_controller))
/// and the runner seam ([`RestateProcessRunner`](super::RestateProcessRunner))
/// both take it, so a segment cannot dispatch an effect before its marker.
#[derive(Debug)]
pub struct SegmentStarted {
    admitted: lash_core::AdmittedScope,
    segment_ordinal: u64,
    started_at_ms: u64,
    authority: ProcessExecutionWriteAuthority,
    generation: Option<Box<lash_core::ExecutableGeneration>>,
    build_generation: Option<Box<lash_core::engine::BuildGeneration>>,
    plugins: Option<Box<lash_core::store::plugin_writers::PluginAdmission>>,
}

impl SegmentStarted {
    fn new(
        process_id: ProcessId,
        segment_ordinal: lash_core::tool_run::SegmentOrdinal,
        started_at_ms: u64,
        execution_id: String,
        generation: Option<lash_core::ExecutableGeneration>,
        build_generation: Option<lash_core::engine::BuildGeneration>,
        plugins: Option<lash_core::store::plugin_writers::PluginAdmission>,
    ) -> Self {
        let authority =
            ProcessExecutionWriteAuthority::invocation(process_id.clone(), execution_id)
                .bind_segment(segment_ordinal);
        Self {
            admitted: lash_core::AdmittedScope::process(process_id),
            segment_ordinal: u64::from(segment_ordinal.0),
            started_at_ms,
            authority,
            generation: generation.map(Box::new),
            build_generation: build_generation.map(Box::new),
            plugins: plugins.map(Box::new),
        }
    }

    /// The plugin admission the segment's start recorded (FIG-4747): the
    /// composition it was admitted under and the writer format chosen for
    /// each plugin then. The segment writes plugin namespaces in these
    /// formats on every execution, never in what the fleet record permits
    /// when it is retried.
    pub fn plugins(&self) -> Option<&lash_core::store::plugin_writers::PluginAdmission> {
        self.plugins.as_deref()
    }

    /// The executable generation the process's start record names: what
    /// the runner's engine must run the segment as (FIG-3571).
    pub fn generation(&self) -> Option<&lash_core::ExecutableGeneration> {
        self.generation.as_deref()
    }

    /// The drain generation the segment's start marker recorded (FIG-3795
    /// S1): the build that admitted it, which the segment's checkpoint
    /// resumes under — never the build now executing.
    pub fn build_generation(&self) -> Option<&lash_core::engine::BuildGeneration> {
        self.build_generation.as_deref()
    }

    /// The process scope the segment's effects are admitted under.
    pub fn admitted_scope(&self) -> &lash_core::AdmittedScope {
        &self.admitted
    }

    /// The segment this proof admits.
    pub fn segment_ordinal(&self) -> u64 {
        self.segment_ordinal
    }

    /// The immutable start marker's time, retained across replay.
    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    /// The execution identity the segment writes its lifecycle facts under.
    pub fn write_authority(&self) -> &ProcessExecutionWriteAuthority {
        &self.authority
    }

    /// A proof for a test that executes a segment without the workflow handler.
    /// The test's own execution authority is kept when it supplies one.
    #[cfg(test)]
    pub(crate) fn for_test(
        admitted: lash_core::AdmittedScope,
        segment_ordinal: u64,
        authority: Option<ProcessExecutionWriteAuthority>,
    ) -> Self {
        let process_id = match admitted.scope() {
            lash_core::ExecutionScope::Process { process_id } => process_id.clone(),
            other => panic!("a segment proof admits a process scope, not {other:?}"),
        };
        Self {
            admitted,
            segment_ordinal,
            started_at_ms: 0,
            authority: authority.unwrap_or_else(|| {
                ProcessExecutionWriteAuthority::invocation(process_id, "test-segment-execution")
            }),
            generation: None,
            build_generation: None,
            plugins: None,
        }
    }
}

/// The boundary policy one segment cuts under, recorded at its admission
/// (FIG-3673): a redeploy that changes the host's selector cannot move a
/// replayed segment's cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SegmentPolicy {
    /// The number of completed effects after which the segment hands over.
    pub(crate) effect_budget: u64,
}

/// What the verdict step journaled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
enum AdmissionVerdict {
    Admit {
        nonce: String,
        /// The digest of the retained handover a later segment resumes from;
        /// `None` for segment 0.
        handover: Option<String>,
        policy: SegmentPolicy,
    },
    SubstrateLost {
        lost: ProcessStarted,
    },
    Superseded {
        latest_segment_ordinal: u64,
    },
    MissingHandover,
    /// The process already holds a terminal: a later segment of an ended
    /// process runs nothing and republishes the stored terminal (FIG-3820).
    Ended {
        output: Box<lash_core::ProcessAwaitOutput>,
    },
    /// The process's own records break an admission invariant: no retry can
    /// admit it, so the segment ends the process Failed (FIG-3819).
    Invariant {
        message: String,
    },
}

/// What the start step journaled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "start", rename_all = "snake_case")]
enum StartOutcome {
    Started {
        execution_id: String,
        process_id: ProcessId,
        started_at_ms: u64,
        /// The executable generation the process's start record names
        /// (FIG-3571), journaled with the start so a replay holds the segment
        /// to the stamp it was admitted under.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<lash_core::ExecutableGeneration>,
        /// The drain generation the segment's start marker recorded
        /// (FIG-3795 S1), journaled with the start so a replay — and any
        /// park the segment writes — sees the checkpoint's stamp, not the
        /// build now executing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_generation: Option<lash_core::engine::BuildGeneration>,
        /// The plugin admission the segment's start recorded (FIG-4747),
        /// journaled with the start so a replay writes plugin namespaces in
        /// the formats chosen at the admission.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugins: Option<lash_core::store::plugin_writers::PluginAdmission>,
    },
    SubstrateLost {
        lost: ProcessStarted,
    },
    /// The process ended between the verdict and the start (FIG-3820).
    Ended {
        output: Box<lash_core::ProcessAwaitOutput>,
    },
    /// See [`AdmissionVerdict::Invariant`].
    Invariant {
        message: String,
    },
}

/// The digest a verdict journals for the handover a segment resumes from.
pub(crate) fn handover_digest(
    handover: &lash_core::SegmentHandover,
) -> Result<String, HandlerError> {
    use sha2::Digest;
    let bytes = serde_json::to_vec(handover)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    Ok(sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// A segment whose start marker committed.
#[derive(Debug)]
pub(crate) struct AdmittedSegment {
    pub(crate) started: SegmentStarted,
    /// The digest of the handover it resumes from, as the verdict recorded.
    pub(crate) handover: Option<String>,
    pub(crate) policy: SegmentPolicy,
    /// The nonce this admission recorded: the writer of the handover the
    /// segment parks, the same on every redrive.
    pub(crate) writer: String,
}

/// How one invocation of a segment proceeds.
#[derive(Debug)]
pub(crate) enum SegmentAdmission {
    /// The marker committed; the segment may run under the recorded policy,
    /// from the handover whose digest the verdict recorded.
    Started(Box<AdmittedSegment>),
    /// A later segment's handover is not retained: nothing can resume it.
    MissingHandover,
    /// The segment started under a journal this invocation cannot read.
    SubstrateLost { lost: ProcessStarted },
    /// The segment already completed; its successor carries the process.
    Superseded { latest_segment_ordinal: u64 },
    /// The process already ended: its stored terminal revokes any later
    /// segment that would carry it on (FIG-3820).
    Ended {
        output: Box<lash_core::ProcessAwaitOutput>,
    },
    /// The process's records break an admission invariant, a fact the
    /// verdict or start step journaled: the segment ends the process Failed
    /// rather than stranding it Running (FIG-3819).
    Invariant { message: String },
}

/// What a segment admission's plugin choice answers: the admitting build's
/// composition and writers, or `None` for a runner that carries no plugins.
pub(crate) type PluginAdmissionFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    Option<lash_core::store::plugin_writers::PluginAdmission>,
                    PluginError,
                >,
            > + Send,
    >,
>;

fn store_fault(error: PluginError) -> HandlerError {
    // Every store read and write here is idempotent, so a fault is retried
    // by Restate rather than failing the invocation.
    HandlerError::from(error)
}

/// The start of the process that owns `segment_ordinal > 0`: its execution
/// is the one every later segment continues.
fn retained_start(record: &ProcessRecord, segment_ordinal: u64) -> Result<ProcessStarted, String> {
    record.first_started.as_deref().cloned().ok_or_else(|| {
        format!(
            "process `{}` segment {segment_ordinal} has a handover without a retained execution start",
            record.id
        )
    })
}

/// An unknown process fails only the invocation: with no record there is no
/// process to strand Running and none to store a terminal on (FIG-3819).
async fn read_record(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &lash_sansio::ProcessId,
) -> Result<ProcessRecord, HandlerError> {
    registry
        .get_process(process_id)
        .await
        .map_err(store_fault)?
        .ok_or_else(|| {
            HandlerError::from(TerminalError::new(format!(
                "unknown process `{process_id}`"
            )))
        })
}

/// Run steps 1 and 2 for `segment_ordinal` of `process_id`.
///
/// These are the handler's first journaled commands; nothing may precede
/// them, and no effect may follow them except under the proof they return.
///
/// `generation` is the executable generation the process's engine runs it as
/// (FIG-3571). Segment 0's start marker *is* the process's start record,
/// which every later attempt and segment inherits, so it names that
/// generation exactly as the runner would; the start returns the stamp the
/// record holds, and a segment whose runner names another is parked before
/// its body runs.
///
/// `build_generation` is the drain generation of the build this admission
/// runs under (FIG-3795 S1): the marker stamps it, and the start journals it
/// so the returned proof carries the recorded stamp, never the executing
/// build's own.
///
/// `plugins` admits the build's plugin composition against the fleet record
/// (FIG-4747). The start step calls it and records the answer on the marker
/// it writes: segment 0's on the process's start record, a later segment's
/// on its own marker, so a child process and a successor each adopt the
/// plugins and writer formats of the build and fleet that admit them. A
/// retry of the step reads the recorded answer back.
///
/// `generation_lane` is the service name this admission runs under when that
/// is a generation lane (FIG-4750): a later segment that starts there is a
/// successor the drain re-sent after the newest build refused it, and its
/// start records the lane on its handover before its marker, so every later
/// cancel, redrive and drain pass addresses the lane the segment runs on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_segment(
    ctx: &WorkflowContext<'_>,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
    process_id: &lash_sansio::ProcessId,
    segment_ordinal: u64,
    generation: Option<lash_core::ExecutableGeneration>,
    build_generation: lash_core::engine::BuildGeneration,
    generation_lane: Option<String>,
    plugins: impl Fn() -> PluginAdmissionFuture + Send + Sync + 'static,
    effect_budget: impl Fn() -> u64 + Send + Sync + 'static,
) -> Result<SegmentAdmission, HandlerError> {
    let run_segment =
        lash_core::tool_run::SegmentOrdinal(u32::try_from(segment_ordinal).map_err(|_| {
            TerminalError::new("process segment ordinal exceeds the Run segment contract")
        })?);
    let effect_budget = Arc::new(effect_budget);
    let plugins = Arc::new(plugins);
    let Json(verdict) = {
        let registry = Arc::clone(registry);
        let continuations = Arc::clone(continuations);
        let process_id = process_id.clone();
        ctx.run(move || {
            let registry = Arc::clone(&registry);
            let continuations = Arc::clone(&continuations);
            let process_id = process_id.clone();
            let effect_budget = Arc::clone(&effect_budget);
            async move {
                let latest = continuations
                    .latest_segment_handover(&process_id)
                    .await
                    .map_err(store_fault)?;
                if let Some(latest) =
                    latest.filter(|latest| latest.segment_ordinal > segment_ordinal)
                {
                    return Ok(Json(AdmissionVerdict::Superseded {
                        latest_segment_ordinal: latest.segment_ordinal,
                    }));
                }
                let handover = if segment_ordinal == 0 {
                    None
                } else {
                    let Some(persisted) = continuations
                        .get_segment_handover(&process_id, segment_ordinal)
                        .await
                        .map_err(store_fault)?
                    else {
                        return Ok(Json(AdmissionVerdict::MissingHandover));
                    };
                    match handover_digest(&persisted.handover) {
                        Ok(digest) => Some(digest),
                        Err(error) => {
                            return Ok(Json(AdmissionVerdict::Invariant {
                                message: format!(
                                    "process `{process_id}` segment {segment_ordinal} handover \
                                     cannot be digested: {error:?}"
                                ),
                            }));
                        }
                    }
                };
                let record = read_record(&registry, &process_id).await?;
                if segment_ordinal > 0
                    && let Some(output) = record.outcome().filter(|_| record.is_terminal())
                {
                    return Ok(Json(AdmissionVerdict::Ended {
                        output: Box::new(output),
                    }));
                }
                let started = if segment_ordinal == 0 {
                    record.first_started.as_deref().cloned()
                } else {
                    let marker = continuations
                        .segment_start(&ProcessSegmentKey::new(process_id.clone(), segment_ordinal))
                        .await
                        .map_err(store_fault)?;
                    match marker {
                        Some(_) => match retained_start(&record, segment_ordinal) {
                            Ok(start) => Some(start),
                            Err(message) => {
                                return Ok(Json(AdmissionVerdict::Invariant { message }));
                            }
                        },
                        None => None,
                    }
                };
                Ok(Json(match started {
                    Some(lost) => AdmissionVerdict::SubstrateLost { lost },
                    None => AdmissionVerdict::Admit {
                        nonce: crate::journaled_nonce(),
                        handover,
                        policy: SegmentPolicy {
                            effect_budget: effect_budget().max(1),
                        },
                    },
                }))
            }
        })
        .name(ADMIT_STEP)
        .await?
    };
    let (nonce, handover, policy) = match verdict {
        AdmissionVerdict::Admit {
            nonce,
            handover,
            policy,
        } => (nonce, handover, policy),
        AdmissionVerdict::MissingHandover => return Ok(SegmentAdmission::MissingHandover),
        AdmissionVerdict::Ended { output } => return Ok(SegmentAdmission::Ended { output }),
        AdmissionVerdict::Invariant { message } => {
            return Ok(SegmentAdmission::Invariant { message });
        }
        AdmissionVerdict::SubstrateLost { lost } => {
            return Ok(SegmentAdmission::SubstrateLost { lost });
        }
        AdmissionVerdict::Superseded {
            latest_segment_ordinal,
        } => {
            return Ok(SegmentAdmission::Superseded {
                latest_segment_ordinal,
            });
        }
    };

    let writer = nonce.clone();
    let Json(start) = {
        let registry = Arc::clone(registry);
        let continuations = Arc::clone(continuations);
        let process_id = process_id.clone();
        ctx.run(move || {
            let registry = Arc::clone(&registry);
            let continuations = Arc::clone(&continuations);
            let process_id = process_id.clone();
            let nonce = nonce.clone();
            let plugins = Arc::clone(&plugins);
            async move {
                if segment_ordinal == 0 {
                    start_root_segment(
                        &registry,
                        &process_id,
                        nonce,
                        generation.clone(),
                        build_generation.clone(),
                        plugins.as_ref(),
                    )
                    .await
                } else {
                    start_later_segment(
                        &registry,
                        &continuations,
                        &process_id,
                        segment_ordinal,
                        nonce,
                        build_generation.clone(),
                        generation_lane.as_deref(),
                        plugins.as_ref(),
                    )
                    .await
                }
                .map(Json)
            }
        })
        .name(START_STEP)
        .await?
    };
    match start {
        StartOutcome::Started {
            execution_id,
            process_id,
            started_at_ms,
            generation,
            build_generation,
            plugins,
        } => Ok(SegmentAdmission::Started(Box::new(AdmittedSegment {
            started: SegmentStarted::new(
                process_id,
                run_segment,
                started_at_ms,
                execution_id,
                generation,
                build_generation,
                plugins,
            ),
            handover,
            policy,
            writer,
        }))),
        StartOutcome::SubstrateLost { lost } => Ok(SegmentAdmission::SubstrateLost { lost }),
        StartOutcome::Ended { output } => Ok(SegmentAdmission::Ended { output }),
        StartOutcome::Invariant { message } => Ok(SegmentAdmission::Invariant { message }),
    }
}

/// Segment 0's marker is the process's `first_started`, bound to the nonce as
/// the execution id every later segment continues.
async fn start_root_segment(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &lash_sansio::ProcessId,
    nonce: String,
    generation: Option<lash_core::ExecutableGeneration>,
    build_generation: lash_core::engine::BuildGeneration,
    plugins: &(impl Fn() -> PluginAdmissionFuture + ?Sized),
) -> Result<StartOutcome, HandlerError> {
    let record = read_record(registry, process_id).await?;
    if record.input.is_externally_owned() {
        // Not an admission invariant: an externally owned process's terminal
        // belongs to its owner, and the workflow-key authority is refused on
        // it, so the invocation fails without writing one (FIG-3819).
        return Err(TerminalError::new(format!(
            "process `{process_id}` is externally owned and is never executed by lash"
        ))
        .into());
    }
    if let Some(existing) = record.first_started.as_deref() {
        // Restate never runs one workflow key twice at once, so the only
        // writer that can have recorded a start since the verdict is this
        // execution's own earlier try of this step.
        return Ok(
            if existing.owner.engine_process_execution_id(process_id) == Some(nonce.as_str()) {
                StartOutcome::Started {
                    execution_id: nonce,
                    process_id: record.id.clone(),
                    started_at_ms: existing.started_at_ms,
                    generation: existing.generation.clone(),
                    build_generation: existing.build_generation.clone(),
                    plugins: existing.plugins.clone(),
                }
            } else {
                StartOutcome::SubstrateLost {
                    lost: existing.clone(),
                }
            },
        );
    }
    let authority = ProcessExecutionWriteAuthority::invocation(process_id.clone(), nonce.clone())
        .bind_attempt(1);
    let Some(mut started) = authority.invocation_started() else {
        return Ok(StartOutcome::Invariant {
            message: format!("process `{process_id}` root segment could not bind its execution"),
        });
    };
    started.started_at_ms = super::restate_now_ms();
    started.generation = generation.clone();
    started.build_generation = Some(build_generation.clone());
    // The process's start is an adoption point (FIG-4747): a child started
    // after a plugin bump records the admitting build's composition and the
    // writer formats the fleet record permits now.
    let plugins = plugins().await.map_err(store_fault)?;
    started.plugins = plugins.clone();
    let recorded = registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .map_err(store_fault)?
        .into_record();
    let Some(start) = recorded.first_started.as_ref() else {
        return Ok(StartOutcome::Invariant {
            message: format!("process `{process_id}` committed no start marker"),
        });
    };
    Ok(StartOutcome::Started {
        execution_id: nonce,
        process_id: record.id.clone(),
        started_at_ms: start.started_at_ms,
        generation: start.generation.clone(),
        build_generation: start.build_generation.clone(),
        plugins: start.plugins.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn start_later_segment(
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
    process_id: &lash_sansio::ProcessId,
    segment_ordinal: u64,
    nonce: String,
    build_generation: lash_core::engine::BuildGeneration,
    generation_lane: Option<&str>,
    plugins: &(impl Fn() -> PluginAdmissionFuture + ?Sized),
) -> Result<StartOutcome, HandlerError> {
    let record = read_record(registry, process_id).await?;
    let root = match retained_start(&record, segment_ordinal) {
        Ok(root) => root,
        Err(message) => return Ok(StartOutcome::Invariant { message }),
    };
    let Some(execution_id) = root
        .owner
        .engine_process_execution_id(process_id)
        .map(str::to_string)
    else {
        return Ok(StartOutcome::Invariant {
            message: format!(
                "process `{process_id}` segment {segment_ordinal} retained a non-Restate execution owner"
            ),
        });
    };
    // A segment starting on a generation lane records the lane as its
    // handover's route first (FIG-4750): once the marker says it started,
    // the recorded route already names where it runs. A retry of this step
    // records the same route again.
    if let Some(lane) = generation_lane {
        continuations
            .record_segment_handover_route(process_id, segment_ordinal, lane)
            .await
            .map_err(store_fault)?;
    }
    // A successor's admission is an adoption point (FIG-4747): it records
    // the admitting build's composition and the writer formats the fleet
    // record permits now. The marker is set-if-absent, so a retry of this
    // step reads the first execution's choice back below.
    let plugins = plugins().await.map_err(store_fault)?;
    // The marker is refused on an ended process in the transaction that
    // writes it, so no terminal lands between the check and the start
    // (FIG-3819). A terminal is permanent: the record read after the refusal
    // carries it.
    let recorded = match continuations
        .mark_segment_started(
            &ProcessSegmentKey::new(process_id.clone(), segment_ordinal),
            SegmentStartMarker {
                nonce: nonce.clone(),
                started_at_ms: super::restate_now_ms(),
                build_generation: Some(build_generation.clone()),
                plugins,
            },
        )
        .await
    {
        Ok(recorded) => recorded,
        Err(PluginError::ProcessAlreadyTerminal { .. }) => {
            let ended = read_record(registry, process_id).await?;
            return match ended.outcome() {
                Some(output) => Ok(StartOutcome::Ended {
                    output: Box::new(output),
                }),
                None => Ok(StartOutcome::Invariant {
                    message: format!(
                        "process `{process_id}` refused segment {segment_ordinal}'s start as \
                         ended but stores no terminal"
                    ),
                }),
            };
        }
        Err(error) => return Err(store_fault(error)),
    };
    Ok(if recorded.nonce == nonce {
        StartOutcome::Started {
            execution_id,
            process_id: record.id.clone(),
            started_at_ms: recorded.started_at_ms,
            generation: root.generation.clone(),
            // The recorded marker's stamp, not the executing build's: a
            // redrive returns the same proof the first execution journaled.
            build_generation: recorded.build_generation,
            plugins: recorded.plugins,
        }
    } else {
        StartOutcome::SubstrateLost { lost: root }
    })
}
