//! A logical call's attempt outcomes and decision: what a round member's
//! `x_outcome` records and how a call ended (ADR 0132 §5). A call's
//! admission, attempts, decision and presentation run in memory inside the
//! admitted execution that makes it durable; nothing here is a journal.

use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use super::material::MaterialRef;

/// The ordinal of an attempt of one logical call, from 1. A crash
/// redelivery keeps it; only a reported retry advances it.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct AttemptOrdinal(NonZeroU32);

impl AttemptOrdinal {
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// The ordinal a reported retry issues next.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for AttemptOrdinal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The physical segment of a logical Run that appended a record, from 0.
/// A successor segment takes the next ordinal when ownership transfers.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SegmentOrdinal(pub u32);

/// What one admitted application attempt settled as (X).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AttemptOutcome {
    Completed(MaterialRef),
    Waiting(CompletionSource),
    Failed(KnownFailure),
    Interrupted,
    TimedOut {
        cause: LimitCause,
        evidence: AvailableEvidence,
    },
    Cancelled {
        evidence: AvailableEvidence,
    },
}

/// What a parked attempt waits on: the tool completion wait its round
/// pinned at admission, and the process terminal wait its resolver pinned
/// when it parked. `metadata` names the parked call's pending completion,
/// whose payload rides in the `x_outcome` record that parks it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionSource {
    /// The tool completion wait, by its id's hex.
    pub wait: String,
    /// The process terminal wait a runtime-owned resolver awaits, by its
    /// id's hex.
    pub terminal: Option<String>,
    /// The parked call's pending completion.
    pub metadata: MaterialRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownFailure {
    pub output: MaterialRef,
    pub reason: KnownFailureReason,
    pub suggested_delay_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownFailureReason {
    Reported,
    DeclarationRefused,
    StartRefused,
}

pub use lash_sansio::LimitCause;

/// Only material actually retained before an attempt stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvailableEvidence {
    pub retained: Option<MaterialRef>,
}

impl AttemptOutcome {
    /// A known failure or slice expiry may use the pinned Repeatable contract.
    /// Failure reasons describe facts; they never carry retry permission.
    pub fn may_repeat(&self) -> bool {
        matches!(
            self,
            Self::Failed(_)
                | Self::TimedOut {
                    cause: LimitCause::ExecutionSlice,
                    ..
                }
        )
    }

    pub fn output(&self) -> Option<&MaterialRef> {
        match self {
            Self::Completed(output) => Some(output),
            Self::Failed(failure) => Some(&failure.output),
            Self::TimedOut { evidence, .. } | Self::Cancelled { evidence } => {
                evidence.retained.as_ref()
            }
            Self::Waiting(_) | Self::Interrupted => None,
        }
    }
}

/// Where a final result came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultSource {
    /// A recorded attempt's Done or Failed output.
    Attempt { attempt: AttemptOrdinal },
    /// The resolved seal of the source the attempt parked on.
    DeferredCompletion {
        attempt: AttemptOrdinal,
        resolved: Box<MaterialRef>,
    },
    /// The cached success the winning before-check supplied.
    Cached,
}

/// The one final-or-cancel decision of a call (D).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallDecision {
    /// The call's result is final. Its declarations, if it `declares`,
    /// drain before its presentation.
    Final {
        source: ResultSource,
        declares: bool,
    },
    /// A check denied the call.
    Denied,
    /// A check cancelled only this call. Its attributed cause stays in the
    /// admission or after-check record; aggregate consumers see a rejection.
    CheckCancelled,
    /// The Run's control cancelled the call, independently of check replies.
    Cancelled,
    /// A check returned AbortRun: the call fails and the Run stops.
    Aborted,
}

impl crate::store::DurableRecord for SegmentOrdinal {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::artifact_referrer::ARTIFACT_REFERRER_KINDS_VERSION);
}
