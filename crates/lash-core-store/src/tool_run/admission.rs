//! K1: whole-round admission (FIG-4875 implements it).
//!
//! One record admits every call of a round before any body starts: the
//! logical owner, each call's identity and operand slots, the final
//! prepared request, the author's three-capability declaration, the
//! executable/preparation/presentation callbacks bound automatically from the
//! plugin composition (FIG-4854's [`PluginCallbackIdentity`]), the recorded
//! runtime retry and cancel policy, the before-check record, and the
//! capacity the round holds. One invalid member admits none. Replay reads
//! this record; it never consults a changed catalog or selects a new route.
//!
//! Binding Q3: there is no per-call timeout, declared duration or budget,
//! and no idempotent capability. The declaration refuses those fields when
//! decoding. Crash recovery is at-least-once under a stable
//! [`ToolCallId`](lash_sansio::ToolCallId) and attempt ordinal, which is the
//! external idempotency key.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use lash_sansio::ToolCallId;
pub use lash_sansio::{DeclarationRefusal, OutcomeShape, ToolDeclaration};
use serde::{Deserialize, Serialize};

use super::material::{MaterialOwner, MaterialRef, MaterialRole};
use super::tool_hooks::{BeforeCheckVerdict, BeforeSelection, CheckRecord};
use crate::effect_opener::EffectOpener;
use crate::store::plugin_writers::{
    PluginCallbackIdentity, PluginExecutionRefusal, PluginRevision,
};

/// The presentation callbacks a call is bound to: the singleton presenter,
/// then the ordered optional steps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationBinding {
    pub presenter: PluginCallbackIdentity,
    pub steps: Vec<PluginCallbackIdentity>,
}

/// The callbacks admission binds automatically, each with its plugin's
/// behavior revision. A resumed call whose bound revision is unavailable
/// refuses typed, before executing anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedBinding {
    /// The tool provider that executes the body.
    pub executable: PluginCallbackIdentity,
    /// The tool provider that prepared the request.
    pub preparation: PluginCallbackIdentity,
    pub presentation: PresentationBinding,
}

impl AdmittedBinding {
    /// Every bound callback, executable first.
    pub fn callbacks(&self) -> impl Iterator<Item = &PluginCallbackIdentity> {
        [
            &self.executable,
            &self.preparation,
            &self.presentation.presenter,
        ]
        .into_iter()
        .chain(&self.presentation.steps)
    }

    /// Refuse when `available` lacks the exact plugin revision of any bound
    /// callback.
    ///
    /// # Errors
    ///
    /// FIG-4854's [`PluginExecutionRefusal`], naming the first unavailable
    /// callback.
    pub fn require_available(
        &self,
        available: &[PluginRevision],
    ) -> Result<(), PluginExecutionRefusal> {
        match self
            .callbacks()
            .find(|callback| !available.contains(&callback.owner))
        {
            None => Ok(()),
            Some(callback) => Err(PluginExecutionRefusal {
                recorded: vec![callback.owner.clone()],
                available: available.to_vec(),
                callback: Some(callback.clone()),
            }),
        }
    }
}

/// The recorded runtime retry policy. A reported failure alone advances
/// the attempt ordinal; a crash redelivers the same attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "retry", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedRetryPolicy {
    #[default]
    Never,
    /// Retry failures the body reports as retryable, up to `max_attempts`
    /// attempts in all, with bounded exponential backoff.
    Reported {
        max_attempts: NonZeroU32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    },
}

/// What cancelling an issued call asks of the external work it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCancelPolicy {
    /// Leave external work running; cancellation ends only the wait.
    #[default]
    Ignore,
    /// Cancel the external work along with the call.
    CancelExternalWork,
}

/// The runtime policy admission records for a call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCallPolicy {
    pub retry: RecordedRetryPolicy,
    pub cancel: ExternalCancelPolicy,
}

/// One admitted logical call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// The final prepared request: after argument transforms and provider
    /// preparation, before any check.
    pub request: MaterialRef,
    pub declaration: ToolDeclaration,
    pub binding: AdmittedBinding,
    pub policy: RuntimeCallPolicy,
    /// Every before-check reply, in reduction order.
    pub checks: CheckRecord<BeforeCheckVerdict>,
}

impl AdmittedCall {
    /// What admission selected for this call.
    #[must_use]
    pub fn selection(&self) -> BeforeSelection {
        self.checks.selection()
    }
}

/// The record that admits one round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundAdmission {
    /// The logical Run's owner; every call id is rooted in its admission.
    pub owner: EffectOpener,
    /// Unique calls in admission order.
    pub members: Vec<AdmittedCall>,
    /// Source operand slots in source order, each the index of its member.
    /// Two slots naming one member are aliases of one call.
    pub operands: Vec<u32>,
}

/// Why a round was refused. One refused member admits no member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmissionRefusal {
    #[error("call {call_id} appears twice in one round")]
    DuplicateCall { call_id: ToolCallId },
    #[error("operand slot {slot} names member {member}, which does not exist")]
    OperandOutOfRange { slot: u32, member: u32 },
    #[error("member {member} is named by no operand slot")]
    UnreferencedMember { member: u32 },
    #[error("member {member}'s request is not a prepared request of the admitting Run")]
    RequestNotOwned { member: u32 },
    #[error("member {member}'s declaration is refused: {cause}")]
    Declaration {
        member: u32,
        cause: DeclarationRefusal,
    },
    #[error("member {member} is isolated and its tool has no process implementation")]
    UnsupportedIsolation { member: u32 },
    #[error("member {member}'s before-check record is not in reduction order")]
    UnreducedChecks { member: u32 },
    #[error("member {member} has a cached result that is not the admitting Run's output")]
    CachedNotOwned { member: u32 },
    #[error("member {member} is bound to an unavailable plugin revision")]
    BindingUnavailable {
        member: u32,
        cause: Box<PluginExecutionRefusal>,
    },
}

/// A round that passed admission: the only way to obtain one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedRound(RoundAdmission);

impl RoundAdmission {
    /// Admit the round against the plugin revisions `available` and the
    /// tools `supports_isolation` names, all members or none.
    ///
    /// The same check guards a recorded round on replay: a recorded round
    /// whose bound revision is now unavailable refuses typed before any
    /// body, route or identity is chosen.
    ///
    /// # Errors
    ///
    /// The first [`AdmissionRefusal`] in member order.
    pub fn admit(
        self,
        available: &[PluginRevision],
        supports_isolation: impl Fn(&str) -> bool,
    ) -> Result<AdmittedRound, AdmissionRefusal> {
        let mut seen = BTreeSet::new();
        for member in &self.members {
            if !seen.insert(&member.call_id) {
                return Err(AdmissionRefusal::DuplicateCall {
                    call_id: member.call_id.clone(),
                });
            }
        }
        let mut referenced = vec![false; self.members.len()];
        for (slot, member) in (0_u32..).zip(&self.operands) {
            let Some(entry) = usize::try_from(*member)
                .ok()
                .and_then(|index| referenced.get_mut(index))
            else {
                return Err(AdmissionRefusal::OperandOutOfRange {
                    slot,
                    member: *member,
                });
            };
            *entry = true;
        }
        let run_owner = MaterialOwner::Run {
            opener: self.owner.clone(),
        };
        for ((member, call), referenced) in (0_u32..).zip(&self.members).zip(referenced) {
            if !referenced {
                return Err(AdmissionRefusal::UnreferencedMember { member });
            }
            if call.request.owner != run_owner || call.request.role != MaterialRole::PreparedRequest
            {
                return Err(AdmissionRefusal::RequestNotOwned { member });
            }
            call.declaration
                .validate()
                .map_err(|cause| AdmissionRefusal::Declaration { member, cause })?;
            if call.declaration.isolated && !supports_isolation(&call.tool_name) {
                return Err(AdmissionRefusal::UnsupportedIsolation { member });
            }
            if !call.checks.is_reduced() {
                return Err(AdmissionRefusal::UnreducedChecks { member });
            }
            let cached_foreign = call.checks.replies().iter().any(|reply| {
                matches!(
                    &reply.verdict,
                    BeforeCheckVerdict::Cached { result }
                        if result.owner != run_owner || result.role != MaterialRole::AttemptOutput
                )
            });
            if cached_foreign {
                return Err(AdmissionRefusal::CachedNotOwned { member });
            }
            call.binding.require_available(available).map_err(|cause| {
                AdmissionRefusal::BindingUnavailable {
                    member,
                    cause: Box::new(cause),
                }
            })?;
        }
        Ok(AdmittedRound(self))
    }
}

impl AdmittedRound {
    /// The admitted record.
    #[must_use]
    pub fn record(&self) -> &RoundAdmission {
        &self.0
    }

    /// The tool-call capacity the round reserves before any member starts:
    /// one per unique call, aliases included once.
    #[must_use]
    pub fn reserved_calls(&self) -> usize {
        self.0.members.len()
    }

    #[must_use]
    pub fn into_record(self) -> RoundAdmission {
        self.0
    }
}
