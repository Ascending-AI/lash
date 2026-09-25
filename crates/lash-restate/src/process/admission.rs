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
//!    the process incarnation it started, so nothing after it reads the record
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
    PluginError, ProcessExecutionWriteAuthority, ProcessRecord, ProcessRef, ProcessRegistry,
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
/// journals around its runner: the admission verdict and the start step above,
/// the segment's recorded cancel races and peeks, and the terminal, boundary
/// and handover steps after it (FIG-3673). Any change to those commands, or to
/// what they key on, bumps it (paused pre-1.0, FIG-3660).
/// Every submitter stamps it on
/// [`RestateProcessWorkflowInput`](super::RestateProcessWorkflowInput), and the
/// handler refuses any other generation before it journals anything. An
/// unstamped input is generation 1, the prefix before FIG-3588.
pub const RESTATE_PROCESS_JOURNAL_VERSION: u32 = 3;

/// The journal name of the verdict step.
const ADMIT_STEP: &str = "lash.segment.admit";
/// The journal name of the start step.
const START_STEP: &str = "lash.segment.start";

/// The generation an input that carries no stamp was written by.
pub(crate) fn unstamped_journal_version() -> u32 {
    1
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
    authority: ProcessExecutionWriteAuthority,
    generation: Option<Box<lash_core::ExecutableGeneration>>,
}

impl SegmentStarted {
    fn new(
        process: ProcessRef,
        segment_ordinal: u64,
        execution_id: String,
        generation: Option<lash_core::ExecutableGeneration>,
    ) -> Self {
        let authority =
            ProcessExecutionWriteAuthority::invocation(process.process_id.clone(), execution_id);
        Self {
            admitted: lash_core::AdmittedScope::process(process),
            segment_ordinal,
            authority,
            generation: generation.map(Box::new),
        }
    }

    /// The executable generation the incarnation's start record names: what
    /// the runner's engine must run the segment as (FIG-3571).
    pub fn generation(&self) -> Option<&lash_core::ExecutableGeneration> {
        self.generation.as_deref()
    }

    /// The process scope the segment's effects are admitted under.
    pub fn admitted_scope(&self) -> &lash_core::AdmittedScope {
        &self.admitted
    }

    /// The segment this proof admits.
    pub fn segment_ordinal(&self) -> u64 {
        self.segment_ordinal
    }

    /// The execution identity the segment writes its lifecycle facts under.
    pub fn write_authority(&self) -> &ProcessExecutionWriteAuthority {
        &self.authority
    }

    /// A proof for a test that drives a segment without the workflow handler.
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
            authority: authority.unwrap_or_else(|| {
                ProcessExecutionWriteAuthority::invocation(process_id, "test-segment-execution")
            }),
            generation: None,
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
        process: ProcessRef,
        /// The executable generation the incarnation's start record names
        /// (FIG-3571), journaled with the start so a replay holds the segment
        /// to the stamp it was admitted under.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<lash_core::ExecutableGeneration>,
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
/// (FIG-3571). Segment 0's start marker *is* the incarnation's start record,
/// which every later attempt and segment inherits, so it names that
/// generation exactly as the runner would; the start returns the stamp the
/// record holds, and a segment whose runner names another is parked before
/// its body runs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_segment(
    ctx: &WorkflowContext<'_>,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
    process_id: &lash_sansio::ProcessId,
    segment_ordinal: u64,
    generation: Option<lash_core::ExecutableGeneration>,
    effect_budget: impl Fn() -> u64 + Send + Sync + 'static,
) -> Result<SegmentAdmission, HandlerError> {
    let effect_budget = Arc::new(effect_budget);
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
                    && let Some(output) = record.outcome.clone().filter(|_| record.is_terminal())
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
                        nonce: uuid::Uuid::new_v4().to_string(),
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
            async move {
                if segment_ordinal == 0 {
                    start_root_segment(&registry, &process_id, nonce, generation.clone()).await
                } else {
                    start_later_segment(
                        &registry,
                        &continuations,
                        &process_id,
                        segment_ordinal,
                        nonce,
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
            process,
            generation,
        } => Ok(SegmentAdmission::Started(Box::new(AdmittedSegment {
            started: SegmentStarted::new(process, segment_ordinal, execution_id, generation),
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
) -> Result<StartOutcome, HandlerError> {
    let record = read_record(registry, process_id).await?;
    if record.disposition == lash_core::RecoveryContract::ExternallyOwned {
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
                    process: ProcessRef::from_record(&record),
                    generation: existing.generation.clone(),
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
    match registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .map_err(store_fault)?
    {
        lash_core::ProcessStartOutcome::Started(_)
        | lash_core::ProcessStartOutcome::AlreadyApplied(_) => Ok(StartOutcome::Started {
            execution_id: nonce,
            process: ProcessRef::from_record(&record),
            generation,
        }),
        lash_core::ProcessStartOutcome::AlreadyStarted { current, .. }
        | lash_core::ProcessStartOutcome::AttemptsExhausted { current, .. } => {
            Ok(match current.first_started.as_deref().cloned() {
                Some(lost) => StartOutcome::SubstrateLost { lost },
                None => StartOutcome::Invariant {
                    message: format!(
                        "process `{process_id}` refused a root start without naming the start it kept"
                    ),
                },
            })
        }
    }
}

async fn start_later_segment(
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
    process_id: &lash_sansio::ProcessId,
    segment_ordinal: u64,
    nonce: String,
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
            },
        )
        .await
    {
        Ok(recorded) => recorded,
        Err(PluginError::ProcessAlreadyTerminal { .. }) => {
            let ended = read_record(registry, process_id).await?;
            return match ended.outcome {
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
            process: ProcessRef::from_record(&record),
            generation: root.generation.clone(),
        }
    } else {
        StartOutcome::SubstrateLost { lost: root }
    })
}
