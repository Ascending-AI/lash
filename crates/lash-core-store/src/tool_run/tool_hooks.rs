//! The tool hook contract the Run records (the adopted hook-composition
//! ruling; FIG-1399 owns the hook API cutover and the ADR 0059 replacement).
//!
//! For one admitted call the sequence is fixed: argument transforms chain in
//! recorded order, the bound provider prepares the call, then every
//! before-check inspects that one immutable prepared call. After the body (or
//! a cached success) result transforms chain, then every after-check
//! inspects the one final candidate. Checks reduce by strength
//! ([`CheckRank`]), ties broken by UTF-8 plugin id and then callback key,
//! never by arrival or registration order.
//!
//! The admission record (A) carries the attributed before verdicts and the
//! selection; the decision record (D) carries the attributed after verdicts.
//! A cached success is data only: it carries no intents, resolver, process
//! start or Run control, and it runs through the result transforms and
//! after-checks like a body result. An after-check returns only
//! Allow/Deny/Cancel/AbortRun; it never replaces a result. AbortRun stops
//! the owning logical Run with a plugin-abort cause.

use std::cmp::Ordering;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use super::material::MaterialRef;
use super::run_event::AttemptOrdinal;
use crate::store::plugin_writers::PluginCallbackIdentity;

/// The four tool hook phases, in execution order around one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolHookPhase {
    /// Argument transforms, before provider preparation.
    ArgsTransform,
    /// Before-checks over the immutable prepared call.
    ArgsCheck,
    /// Result transforms over the original and the preceding candidate.
    ResultTransform,
    /// After-checks over the one final candidate.
    ResultCheck,
}

impl ToolHookPhase {
    /// Whether a callback of this phase may propose plugin-state commands
    /// (binding hook policy). Only after-checks, which run on the Run's
    /// sequential result-publication path, may; transforms and
    /// before-checks are decision-only.
    #[must_use]
    pub const fn may_propose_state_commands(self) -> bool {
        matches!(self, Self::ResultCheck)
    }
}

/// Which evaluation of a call a hook reply belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "occurrence", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolHookOccurrence {
    /// The call's admission: argument transforms and before-checks run once
    /// per logical call, never again on a reported retry.
    Admission,
    /// The result of one attempt. A reported retry is a new occurrence; a
    /// crash redelivery of the same attempt is the same occurrence.
    Attempt { attempt: AttemptOrdinal },
    /// The authenticated completion of the Deferred source an attempt
    /// parked on.
    DeferredCompletion { attempt: AttemptOrdinal },
    /// A cached success a before-check supplied: no attempt executed.
    Cached,
}

impl ToolHookOccurrence {
    /// Whether `phase` can run at this occurrence.
    #[must_use]
    pub const fn admits(self, phase: ToolHookPhase) -> bool {
        match self {
            Self::Admission => matches!(
                phase,
                ToolHookPhase::ArgsTransform | ToolHookPhase::ArgsCheck
            ),
            Self::Attempt { .. } | Self::DeferredCompletion { .. } | Self::Cached => matches!(
                phase,
                ToolHookPhase::ResultTransform | ToolHookPhase::ResultCheck
            ),
        }
    }
}

/// The identity of one callback's evaluation at one occurrence: the dedup
/// key of its recorded reply and of any state commands it proposed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookOccurrence {
    pub call_id: lash_sansio::ToolCallId,
    pub callback: PluginCallbackIdentity,
    pub phase: ToolHookPhase,
    pub occurrence: ToolHookOccurrence,
}

/// A recorded typed hook cause: a plugin's declared error type, its version
/// and payload. Unknown payloads survive replay unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookCause {
    pub error_type: String,
    pub error_version: NonZeroU32,
    pub payload: serde_json::Value,
}

/// The strength a check reply reduces by. Higher wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRank {
    Allow,
    CachedSuccess,
    /// Deny and Cancel share a rank and keep distinct typed outcomes.
    DenyOrCancel,
    AbortRun,
}

/// A before-check's reply, as admission records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case", deny_unknown_fields)]
pub enum BeforeCheckVerdict {
    Allow,
    /// A successful result in place of the body: the payload only, owned by
    /// the admitting Run as attempt output.
    Cached {
        result: MaterialRef,
    },
    /// Fail the call.
    Deny {
        cause: HookCause,
    },
    /// Cancel the call.
    Cancel {
        cause: HookCause,
    },
    /// Fail the call and stop the owning logical Run.
    AbortRun {
        cause: HookCause,
    },
}

/// An after-check's reply. It has no variant that carries a result: an
/// after-check cannot replace one. Recovery belongs in a result transform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case", deny_unknown_fields)]
pub enum AfterCheckVerdict {
    Allow,
    Deny { cause: HookCause },
    Cancel { cause: HookCause },
    AbortRun { cause: HookCause },
}

/// A check reply the reducer can rank.
pub trait RankedVerdict {
    fn rank(&self) -> CheckRank;
}

impl RankedVerdict for BeforeCheckVerdict {
    fn rank(&self) -> CheckRank {
        match self {
            Self::Allow => CheckRank::Allow,
            Self::Cached { .. } => CheckRank::CachedSuccess,
            Self::Deny { .. } | Self::Cancel { .. } => CheckRank::DenyOrCancel,
            Self::AbortRun { .. } => CheckRank::AbortRun,
        }
    }
}

impl RankedVerdict for AfterCheckVerdict {
    fn rank(&self) -> CheckRank {
        match self {
            Self::Allow => CheckRank::Allow,
            Self::Deny { .. } | Self::Cancel { .. } => CheckRank::DenyOrCancel,
            Self::AbortRun { .. } => CheckRank::AbortRun,
        }
    }
}

/// One callback's reply, attributed to the callback that gave it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributedVerdict<V> {
    pub callback: PluginCallbackIdentity,
    pub verdict: V,
}

/// The reducer order: stronger first, then ascending UTF-8 plugin id, then
/// ascending callback key.
fn reduction_order<V: RankedVerdict>(
    left: &AttributedVerdict<V>,
    right: &AttributedVerdict<V>,
) -> Ordering {
    right
        .verdict
        .rank()
        .cmp(&left.verdict.rank())
        .then_with(|| {
            left.callback
                .owner
                .plugin
                .as_bytes()
                .cmp(right.callback.owner.plugin.as_bytes())
        })
        .then_with(|| {
            left.callback
                .key
                .as_bytes()
                .cmp(right.callback.key.as_bytes())
        })
}

/// Every reply of one phase, in reduction order, with the winner first.
///
/// Every check is called, sequentially, against one snapshot; this record
/// keeps all their replies so the selected decision and the full sorted
/// evidence derive from the same data. The order does not depend on the
/// order replies arrived in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckRecord<V>(Vec<AttributedVerdict<V>>);

impl<V: RankedVerdict> CheckRecord<V> {
    /// Reduce `replies`, received in any order.
    #[must_use]
    pub fn reduce(mut replies: Vec<AttributedVerdict<V>>) -> Self {
        replies.sort_by(reduction_order);
        Self(replies)
    }

    /// The selected reply; `None` for a phase with no checks, which allows.
    #[must_use]
    pub fn winner(&self) -> Option<&AttributedVerdict<V>> {
        self.0.first()
    }

    /// Every reply in reduction order.
    #[must_use]
    pub fn replies(&self) -> &[AttributedVerdict<V>] {
        &self.0
    }

    /// Whether a recorded record is in reduction order: a replayed record is
    /// served as recorded and never re-reduced, so a disordered one is
    /// refused rather than silently re-sorted.
    #[must_use]
    pub fn is_reduced(&self) -> bool {
        self.0
            .windows(2)
            .all(|pair| reduction_order(&pair[0], &pair[1]) != Ordering::Greater)
    }
}

/// What admission selected for a call from its before-check record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeforeSelection {
    /// Issue attempt 1 of the body.
    Execute,
    /// Use the winning cached result; no attempt is issued.
    Cached,
    /// Fail the call; no attempt is issued.
    Deny,
    /// Cancel the call; no attempt is issued.
    Cancel,
    /// Fail the call and stop the Run; no attempt is issued.
    AbortRun,
}

impl CheckRecord<BeforeCheckVerdict> {
    /// The admission selection the record's winner implies.
    #[must_use]
    pub fn selection(&self) -> BeforeSelection {
        match self.winner().map(|reply| &reply.verdict) {
            None | Some(BeforeCheckVerdict::Allow) => BeforeSelection::Execute,
            Some(BeforeCheckVerdict::Cached { .. }) => BeforeSelection::Cached,
            Some(BeforeCheckVerdict::Deny { .. }) => BeforeSelection::Deny,
            Some(BeforeCheckVerdict::Cancel { .. }) => BeforeSelection::Cancel,
            Some(BeforeCheckVerdict::AbortRun { .. }) => BeforeSelection::AbortRun,
        }
    }
}
