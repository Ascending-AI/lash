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
//!    is superseded, never refused.
//! 2. **Start** (`lash.segment.start`). Record the marker with that nonce,
//!    set-if-absent. The recorded nonce equals ours: this execution's marker,
//!    written now or by this execution's own earlier try. A different nonce: an
//!    execution this one does not continue started the segment, so the process
//!    ends `SubstrateLost`.
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

/// The generation of the Restate process handler's journaled command prefix.
///
/// This version owns the leading commands every `LashProcessWorkflow/run`
/// invocation journals: today the admission verdict and the start step above.
/// Any change to those leading commands, or to what they key on, bumps it.
/// Every submitter stamps it on
/// [`RestateProcessWorkflowInput`](super::RestateProcessWorkflowInput), and the
/// handler refuses any other generation before it journals anything. An
/// unstamped input is generation 1, the prefix before FIG-3588.
pub const RESTATE_PROCESS_JOURNAL_VERSION: u32 = 2;

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
}

impl SegmentStarted {
    fn new(process: ProcessRef, segment_ordinal: u64, execution_id: String) -> Self {
        let authority =
            ProcessExecutionWriteAuthority::invocation(process.process_id.clone(), execution_id);
        Self {
            admitted: lash_core::AdmittedScope::process(process),
            segment_ordinal,
            authority,
        }
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
        }
    }
}

/// What the verdict step journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
enum AdmissionVerdict {
    Admit { nonce: String },
    SubstrateLost { lost: ProcessStarted },
    Superseded { latest_segment_ordinal: u64 },
}

/// What the start step journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "start", rename_all = "snake_case")]
enum StartOutcome {
    Started { execution_id: String },
    SubstrateLost { lost: ProcessStarted },
}

/// How one invocation of a segment proceeds.
#[derive(Debug)]
pub(crate) enum SegmentAdmission {
    /// The marker committed; the segment may run.
    Started(SegmentStarted),
    /// The segment started under a journal this invocation cannot read.
    SubstrateLost { lost: ProcessStarted },
    /// The segment already completed; its successor carries the process.
    Superseded { latest_segment_ordinal: u64 },
}

fn store_fault(error: PluginError) -> HandlerError {
    // Every store read and write here is idempotent, so a fault is retried
    // by Restate rather than failing the invocation.
    HandlerError::from(error)
}

/// The start of the process that owns `segment_ordinal > 0`: its execution
/// is the one every later segment continues.
fn retained_start(
    record: &ProcessRecord,
    segment_ordinal: u64,
) -> Result<ProcessStarted, HandlerError> {
    record.first_started.as_deref().cloned().ok_or_else(|| {
        HandlerError::from(TerminalError::new(format!(
            "process `{}` segment {segment_ordinal} has a handover without a retained execution start",
            record.id
        )))
    })
}

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
/// `replay_grammar` is the replay-key grammar the process's engine journals
/// under (FIG-3586). Segment 0's start marker *is* the incarnation's start
/// record, which every later attempt and segment inherits, so it must name
/// that grammar exactly as the runner would; an unstamped record is refused
/// by an engine that keys its journal by grammar, before its body runs.
pub(crate) async fn admit_segment(
    ctx: &WorkflowContext<'_>,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
    process_id: &lash_sansio::ProcessId,
    segment_ordinal: u64,
    replay_grammar: Option<u32>,
) -> Result<SegmentAdmission, HandlerError> {
    let Json(verdict) = {
        let registry = Arc::clone(registry);
        let continuations = Arc::clone(continuations);
        let process_id = process_id.clone();
        ctx.run(move || {
            let registry = Arc::clone(&registry);
            let continuations = Arc::clone(&continuations);
            let process_id = process_id.clone();
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
                let record = read_record(&registry, &process_id).await?;
                let started = if segment_ordinal == 0 {
                    record.first_started.as_deref().cloned()
                } else {
                    let marker = continuations
                        .segment_start(&ProcessSegmentKey::new(process_id.clone(), segment_ordinal))
                        .await
                        .map_err(store_fault)?;
                    match marker {
                        Some(_) => Some(retained_start(&record, segment_ordinal)?),
                        None => None,
                    }
                };
                Ok(Json(match started {
                    Some(lost) => AdmissionVerdict::SubstrateLost { lost },
                    None => AdmissionVerdict::Admit {
                        nonce: uuid::Uuid::new_v4().to_string(),
                    },
                }))
            }
        })
        .name(ADMIT_STEP)
        .await?
    };
    let nonce = match verdict {
        AdmissionVerdict::Admit { nonce } => nonce,
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
                    start_root_segment(&registry, &process_id, nonce, replay_grammar).await
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
        StartOutcome::Started { execution_id } => {
            let record = read_record(registry, process_id).await?;
            Ok(SegmentAdmission::Started(SegmentStarted::new(
                ProcessRef::from_record(&record),
                segment_ordinal,
                execution_id,
            )))
        }
        StartOutcome::SubstrateLost { lost } => Ok(SegmentAdmission::SubstrateLost { lost }),
    }
}

/// Segment 0's marker is the process's `first_started`, bound to the nonce as
/// the execution id every later segment continues.
async fn start_root_segment(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &lash_sansio::ProcessId,
    nonce: String,
    replay_grammar: Option<u32>,
) -> Result<StartOutcome, HandlerError> {
    let record = read_record(registry, process_id).await?;
    if record.disposition == lash_core::RecoveryContract::ExternallyOwned {
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
            if existing.owner.restate_process_execution_id(process_id) == Some(nonce.as_str()) {
                StartOutcome::Started {
                    execution_id: nonce,
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
    let mut started = authority.invocation_started().ok_or_else(|| {
        HandlerError::from(TerminalError::new(format!(
            "process `{process_id}` root segment could not bind its execution"
        )))
    })?;
    started.started_at_ms = super::restate_now_ms();
    started.replay_grammar = replay_grammar;
    match registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .map_err(store_fault)?
    {
        lash_core::ProcessStartOutcome::Started(_)
        | lash_core::ProcessStartOutcome::AlreadyApplied(_) => Ok(StartOutcome::Started {
            execution_id: nonce,
        }),
        lash_core::ProcessStartOutcome::AlreadyStarted { current, .. }
        | lash_core::ProcessStartOutcome::AttemptsExhausted { current, .. } => {
            let lost = current.first_started.as_deref().cloned().ok_or_else(|| {
                HandlerError::from(TerminalError::new(format!(
                    "process `{process_id}` refused a root start without naming the start it kept"
                )))
            })?;
            Ok(StartOutcome::SubstrateLost { lost })
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
    let root = retained_start(&record, segment_ordinal)?;
    let execution_id = root
        .owner
        .restate_process_execution_id(process_id)
        .ok_or_else(|| {
            HandlerError::from(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} retained a non-Restate execution owner"
            )))
        })?
        .to_string();
    let recorded = continuations
        .mark_segment_started(
            &ProcessSegmentKey::new(process_id.clone(), segment_ordinal),
            SegmentStartMarker {
                nonce: nonce.clone(),
                started_at_ms: super::restate_now_ms(),
            },
        )
        .await
        .map_err(store_fault)?;
    Ok(if recorded.nonce == nonce {
        StartOutcome::Started { execution_id }
    } else {
        StartOutcome::SubstrateLost { lost: root }
    })
}
