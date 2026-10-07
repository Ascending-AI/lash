//! A turn's tool round: the tools its members run, pinned at admission from
//! the catalog, and the members' bodies and answers.
//!
//! The round's rows are the coordinator's records: the admission pins each
//! member's tool, request, policy and limit; a member's body runs its call's
//! admission checks, its attempt and its decision in memory between its
//! `x_start` and its `x_outcome`; and the outcome's material is what the
//! turn's machine is answered with. A resume rebuilds the round from its
//! rows ([`fold`](super::fold)) and re-runs nothing it recorded.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AvailableEvidence, CompletionSource, MaterialLocation, MaterialOwner, MaterialPayload,
    MaterialRef, MaterialRole,
};
use lash_durable::ActorTx;
use lash_durable::domain::{OwnerKey, RunSeq};
use lash_sansio::sansio::PendingToolCall;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};

use super::super::ActorContext;
use super::super::waits::{Resolution, WaitDeadline};
use super::{
    AdmittedExecution, ExecutionDraft, Material, MemberBodies, MemberBody, PolicyView, RoundError,
    SettledOutput, fold, settle,
};
use crate::{ToolCallId, ToolId};

/// A settled member as the turn's machine is answered with it.
pub type CompletedCall = lash_sansio::sansio::CompletedToolCall<crate::ToolIntentExecutionOutcome>;

/// What the catalog pins a member to at admission: never refreshed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberPin {
    /// The tool the call names.
    pub tool: ToolId,
    /// Its declared execution policy.
    pub policy: ExecutionPolicy,
    /// Its limit, starting now.
    pub limit: ExecutionLimit,
    /// For a tool that may park, the deadline of the completion wait its
    /// round pins: the call's park never outlives it.
    pub wait: Option<WaitDeadline>,
}

/// The tools a turn's rounds run: the catalog the turn was built with.
///
/// Nothing here drives the turn or commits: the phase runner admits, runs
/// and presents the round, and asks this for what only the catalog knows.
pub trait RoundTools: Send + Sync {
    /// What `call` is admitted as, read from the catalog at `now_ms`. Runs
    /// no hook, preparation or body.
    fn pin(&self, call: &PendingToolCall, now_ms: u64) -> MemberPin;

    /// The policies the catalog declares now: what a resumed round vetoes a
    /// stored repeat against.
    fn policies(&self) -> PolicyView;

    /// The body of `execution`, an attempt of `call`: the call's admission
    /// checks, its attempt and its decision, run in memory. Its answer is
    /// the attempt's outcome, the journal-local material it names and the
    /// store-local effect that commits with a completion.
    fn body(&self, call: &PendingToolCall, execution: &AdmittedExecution) -> MemberBody;

    /// The final answer of `execution`, an attempt of `call` that parked as
    /// `parked`, once one of its waits ended with `resolution`: a pure
    /// function of the resolution and the parked call's pending completion,
    /// as its `Waiting` outcome recorded it. Runs no body.
    fn resolved(
        &self,
        call: &PendingToolCall,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput;

    /// Release what `execution`, an attempt of `call`, launched for its
    /// park, once the park ended: `cancelled` when the call ends cancelled.
    /// See [`MemberBodies::discharge`](super::MemberBodies::discharge).
    fn discharge<'a>(
        &'a self,
        _call: &'a PendingToolCall,
        _execution: &'a AdmittedExecution,
        _parked: &'a Material<CompletionSource>,
        _cancelled: bool,
    ) -> super::lifecycle::Discharge<'a> {
        Box::pin(async {})
    }

    /// What the machine is answered with for `call`: a pure function of its
    /// committed `output`, with the payload of the material it names.
    fn completed(&self, call: &PendingToolCall, output: &SettledOutput) -> CompletedCall;

    /// The round's admission refusal, read from the catalog: when it refuses
    /// any member of `calls`, what every member answers instead, in declared
    /// order. A refused round's members settle at its admission and no body
    /// runs. Runs no hook, preparation or body.
    fn refusal(&self, _calls: &[PendingToolCall]) -> Option<Vec<CompletedCall>> {
        None
    }
}

/// A member's answer as journal-local material owned by `owner`'s turn: the
/// output a completion or a known failure names.
///
/// # Errors
///
/// [`RoundCallsRefusal::Unencodable`].
pub fn completed_material(
    owner: &EffectOpener,
    completed: &CompletedCall,
) -> Result<Material, RoundCallsRefusal> {
    let text = serde_json::to_string(completed)
        .map_err(|_| RoundCallsRefusal::Unencodable(completed.call_id.clone()))?;
    Ok(Material::journal_local(
        MaterialOwner::Run {
            opener: owner.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    ))
}

/// The answer [`completed_material`] encoded.
#[must_use]
pub fn decode_completed(material: &str) -> Option<CompletedCall> {
    serde_json::from_str(material).ok()
}

/// Why a round's calls do not match its admission.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoundCallsRefusal {
    /// The calls the checkpoint re-delivered are not the ones the round
    /// admitted, in its declared order.
    #[error("the re-delivered calls are not the ones run {run:?} admitted")]
    Drift {
        /// The round's run.
        run: RunSeq,
    },
    /// A call's request does not encode.
    #[error("call {0}'s request does not encode")]
    Unencodable(ToolCallId),
}

/// The request material of `call`, owned by `owner`'s turn: the call as the
/// model issued it, by digest. A round admits it and a resume compares the
/// re-delivered call against it.
///
/// # Errors
///
/// [`RoundCallsRefusal::Unencodable`].
pub fn request_material(
    owner: &EffectOpener,
    call: &PendingToolCall,
) -> Result<MaterialRef, RoundCallsRefusal> {
    let unencodable = || RoundCallsRefusal::Unencodable(call.call_id.clone());
    let text = serde_json::to_string(call).map_err(|_| unencodable())?;
    MaterialPayload::new(
        MaterialOwner::Run {
            opener: owner.clone(),
        },
        MaterialRole::PreparedRequest,
        None,
        text,
    )
    .reference(MaterialLocation::JournalLocal)
    .map_err(|_| unencodable())
}

/// The draft `call` is admitted as in `owner`'s turn, pinned as `pin` says.
///
/// # Errors
///
/// [`RoundCallsRefusal::Unencodable`].
pub fn call_draft(
    owner: &EffectOpener,
    call: &PendingToolCall,
    pin: MemberPin,
) -> Result<ExecutionDraft, RoundCallsRefusal> {
    Ok(ExecutionDraft::new(
        call.call_id.clone(),
        pin.tool,
        request_material(owner, call)?,
        pin.policy,
        pin.limit,
        pin.wait,
    ))
}

/// The member bodies of one round: each admitted execution runs the body
/// `tools` gives its call.
pub struct RoundCalls {
    tools: Arc<dyn RoundTools>,
    calls: BTreeMap<ToolCallId, PendingToolCall>,
}

impl RoundCalls {
    /// The bodies of `calls` from `tools`.
    #[must_use]
    pub fn new(tools: Arc<dyn RoundTools>, calls: &[PendingToolCall]) -> Self {
        Self {
            tools,
            calls: calls
                .iter()
                .map(|call| (call.call_id.clone(), call.clone()))
                .collect(),
        }
    }
}

impl MemberBodies for RoundCalls {
    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        match self.calls.get(execution.call()) {
            Some(call) => self.tools.resolved(call, execution, parked, resolution),
            None => SettledOutput::Cancelled {
                evidence: AvailableEvidence::default(),
            },
        }
    }

    fn discharge<'a>(
        &'a self,
        execution: &'a AdmittedExecution,
        parked: &'a Material<CompletionSource>,
        cancelled: bool,
    ) -> super::lifecycle::Discharge<'a> {
        match self.calls.get(execution.call()) {
            Some(call) => self.tools.discharge(call, execution, parked, cancelled),
            None => Box::pin(async {}),
        }
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.calls.get(execution.call()) {
            Some(call) => self.tools.body(call, execution),
            // The round admitted only these calls; a body for any other is
            // never asked for. Answer as a stop rather than run anything.
            None => Box::new(|_| {
                Box::pin(async {
                    SettledOutput::Cancelled {
                        evidence: AvailableEvidence::default(),
                    }
                    .into()
                })
            }),
        }
    }
}

/// Whether `calls` are the ones `drafts` admitted, in order, each with the
/// request it was admitted with.
///
/// # Errors
///
/// [`RoundCallsRefusal`].
pub fn require_admitted(
    owner: &EffectOpener,
    run: RunSeq,
    drafts: &[&ExecutionDraft],
    calls: &[PendingToolCall],
) -> Result<(), RoundCallsRefusal> {
    if drafts.len() != calls.len() {
        return Err(RoundCallsRefusal::Drift { run });
    }
    for (draft, call) in drafts.iter().zip(calls) {
        if draft.call() != &call.call_id || draft.request() != &request_material(owner, call)? {
            return Err(RoundCallsRefusal::Drift { run });
        }
    }
    Ok(())
}

/// Record `Cancelled` for every member of `owner`'s run `run` that has no
/// final outcome, on `tx`: what a turn cancel leaves of a round whose owner
/// stopped before its members settled. Runs no body. A run that was never
/// admitted, or whose members all settled, records nothing.
///
/// # Errors
///
/// [`RoundError`] when the rows cannot be read or do not fold.
pub async fn settle_cancelled(
    cx: &ActorContext,
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
) -> Result<(), RoundError> {
    let rows: Vec<_> = cx
        .durable_reads()?
        .run_records(owner)
        .await?
        .into_iter()
        .filter(|row| row.run == run)
        .collect();
    let folded = fold(&rows, &PolicyView::default())?;
    let Some(view) = folded.round(run) else {
        return Ok(());
    };
    for member in view.members() {
        if member.outcome().is_none() {
            settle(
                tx,
                &view.execution(member),
                SettledOutput::Cancelled {
                    evidence: AvailableEvidence::default(),
                },
                Vec::new(),
            )?;
        }
    }
    Ok(())
}
