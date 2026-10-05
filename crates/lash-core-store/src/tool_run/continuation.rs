//! K6: a physical cut transfers the complete logical Run (FIG-4881,
//! FIG-4739, FIG-4890 and FIG-4889 implement it).
//!
//! A cut moves through RequestCut, Quiescing and Capturable. RequestCut
//! stops new admission. Quiescing keeps polling until every issued local
//! attempt is durably acknowledged; a Deferred source does not hold the cut,
//! its subscription transfers instead. Capturable then transfers, in one
//! ownership move, every Run event, retained material, pending source,
//! owed launch and cancel. The journal derives capacity and applied state;
//! the boundary container carries its reason and VM continuation. No native future, socket or borrowed context
//! transfers. Turn and process segments share this record and its laws and
//! keep their own lifecycle transactions.
//!
//! The transfer is bound to its logical Run's owner (FIG-4861): a fresh Run
//! that executes identical source adopts nothing from it, and a terminal
//! Run's transfer is adopted by nobody.

use lash_sansio::BoundaryReason;
use serde::{Deserialize, Serialize};

use super::retention::{MaterialHolder, RetainedBundle};
use super::run_event::{
    RunAttemptEntry, RunEventRefusal, RunJournalEntry, RunLedger, RunLifecycle, SegmentOrdinal,
};
use super::source_seal::SourceDescriptor;
use crate::await_event_identity::AwaitEventKey;
use crate::effect_opener::EffectOpener;
use crate::process_identity::ProcessExecutionEnvRef;

/// Where a requested cut stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutPhase {
    /// Admission is stopped; issued work continues.
    RequestCut,
    /// Waiting for the durable acknowledgement of issued local attempts.
    Quiescing,
    /// Nothing local is unacknowledged; the transfer may be captured.
    Capturable,
}

/// A requested physical cut of a logical Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    pub reason: BoundaryReason,
    pub phase: CutPhase,
}

impl Cut {
    /// A cut just requested for `reason`.
    #[must_use]
    pub fn request(reason: BoundaryReason) -> Self {
        Self {
            reason,
            phase: CutPhase::RequestCut,
        }
    }

    /// Whether the Run may admit new work: never once a cut is requested.
    #[must_use]
    pub const fn admits_new_work(&self) -> bool {
        false
    }

    /// The phase after observing `unacknowledged_local` issued local
    /// attempts without a durable acknowledgement. Deferred sources are not
    /// counted: they transfer pending.
    #[must_use]
    pub fn observe(self, unacknowledged_local: usize) -> Self {
        Self {
            phase: if unacknowledged_local == 0 {
                CutPhase::Capturable
            } else {
                CutPhase::Quiescing
            },
            ..self
        }
    }
}

/// Everything a cut hands its successor segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTransfer {
    /// The logical Run's owner address.
    pub owner: EffectOpener,
    pub from: SegmentOrdinal,
    /// Acknowledged admission, selection, decision, drain and consumption facts.
    pub entries: Vec<RunJournalEntry>,
    /// Acknowledged independent X receipts, in recorded selection order.
    pub attempts: Vec<RunAttemptEntry>,
    /// Original journal references, resolved through the retained bundles by
    /// owner, role and digest. Relocation does not change their identity.
    pub material_aliases: Vec<super::material::MaterialRef>,
    /// The sources' immutable resolver and cancellation authority.
    pub sources: Vec<SourceDescriptor>,
    /// The environment admitted by this logical Run, unchanged by a cut.
    pub environment: Option<ProcessExecutionEnvRef>,
    /// Published namespaces and their applied frontiers, including initialization.
    pub plugin_state: Option<crate::plugin_state::PluginState>,
    /// Every unconsumed payload, in bundles retained under the transferring
    /// segment's lease before publication. The successor acquires its own
    /// lease on each before the predecessor releases.
    pub material: Vec<TransferBundle>,
    /// Pending sources, rebound to the successor on adoption.
    pub subscriptions: Vec<AwaitEventKey>,
}

/// Why a transfer cannot be captured or adopted.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationRefusal {
    #[error("the captured Run records were refused: {cause}")]
    Records { cause: RunEventRefusal },
    #[error("the captured segment frontier does not name its writer")]
    SegmentFrontier,
    #[error("the process continuation has no admitted segment authority")]
    MissingSegmentAuthority,
    #[error("the Run's environment differs from its admitted process environment")]
    ForeignEnvironment,
    #[error("the capture still owes a local attempt acknowledgement")]
    UnacknowledgedAttempt,
    #[error("the source descriptors do not belong to this Run")]
    ForeignSource,
    #[error("the cut is not capturable yet")]
    NotQuiescent,
    #[error("material is journal-local and cannot cross segments")]
    UnretainedMaterial,
    #[error("the transfer belongs to another logical Run")]
    ForeignOwner,
    #[error("the logical Run is terminal; its transfer is void")]
    OwnerTerminal,
    #[error("segment {found} is not the successor of segment {from}")]
    NotSuccessor { from: u32, found: u32 },
}

impl From<ContinuationRefusal> for crate::runtime_error::RuntimeEffectControllerError {
    fn from(refusal: ContinuationRefusal) -> Self {
        let mut error = Self::new(
            crate::RuntimeErrorCode::ExecutionStateCaptureFailed,
            refusal.to_string(),
        );
        error.cause = Some(crate::RuntimeErrorCause::RunContinuationRefused {
            refusal: Box::new(refusal),
        });
        error
    }
}

impl RunTransfer {
    /// Rebuild the fold from acknowledged records, without executing a body.
    ///
    /// # Errors
    /// A malformed event stream.
    pub fn ledger(&self) -> Result<RunLedger, ContinuationRefusal> {
        let mut ledger = RunLedger::new(self.owner.clone());
        for entry in &self.entries {
            ledger
                .append(entry.record.segment, &entry.record)
                .map_err(|cause| ContinuationRefusal::Records { cause })?;
        }
        ledger.admit_successor(self.from);
        Ok(ledger)
    }

    /// Check a transfer captured under `cut`.
    ///
    /// # Errors
    ///
    /// [`ContinuationRefusal`] for a cut still quiescing, material outside
    /// a retained bundle, or a malformed journal.
    pub fn check_capture(&self, phase: CutPhase) -> Result<(), ContinuationRefusal> {
        if phase != CutPhase::Capturable {
            return Err(ContinuationRefusal::NotQuiescent);
        }
        if !self.material.iter().all(TransferBundle::is_retained) {
            return Err(ContinuationRefusal::UnretainedMaterial);
        }
        if self.sources.iter().any(|source| source.owner != self.owner) {
            return Err(ContinuationRefusal::ForeignSource);
        }
        if self.material_aliases.iter().any(|alias| {
            !self
                .material
                .iter()
                .flat_map(|bundle| &bundle.references)
                .any(|reference| {
                    reference.owner == alias.owner
                        && reference.role == alias.role
                        && reference.digest == alias.digest
                })
        }) {
            return Err(ContinuationRefusal::UnretainedMaterial);
        }
        let ledger = self.ledger()?;
        for call_id in ledger.owed_realizations() {
            if ledger.realization_invocation(&call_id).is_none() {
                return Err(ContinuationRefusal::Records {
                    cause: RunEventRefusal::RealizationOwed { call_id },
                });
            }
        }
        if ledger.unacknowledged_local() != 0 {
            return Err(ContinuationRefusal::UnacknowledgedAttempt);
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.record.segment > self.from)
        {
            return Err(ContinuationRefusal::SegmentFrontier);
        }
        Ok(())
    }

    /// The lease holder of the transferring segment.
    #[must_use]
    pub fn holder(&self) -> MaterialHolder {
        MaterialHolder::Segment {
            opener: self.owner.clone(),
            segment: self.from,
        }
    }

    /// Adopt the transfer into segment `successor` of the Run `owner`,
    /// whose recorded lifecycle is `lifecycle`; the subscriptions move to the
    /// successor.
    ///
    /// # Errors
    ///
    /// [`ContinuationRefusal::ForeignOwner`] for a fresh or different Run,
    /// [`ContinuationRefusal::OwnerTerminal`] once the Run settled or was
    /// cancelled, and [`ContinuationRefusal::NotSuccessor`].
    pub fn adopt(
        self,
        owner: &EffectOpener,
        lifecycle: RunLifecycle,
        successor: SegmentOrdinal,
    ) -> Result<AdoptedRun, ContinuationRefusal> {
        if &self.owner != owner {
            return Err(ContinuationRefusal::ForeignOwner);
        }
        if lifecycle == RunLifecycle::Settled {
            return Err(ContinuationRefusal::OwnerTerminal);
        }
        if self.from.0.checked_add(1) != Some(successor.0) {
            return Err(ContinuationRefusal::NotSuccessor {
                from: self.from.0,
                found: successor.0,
            });
        }
        Ok(AdoptedRun {
            transfer: self,
            successor,
        })
    }
}

/// Successor ownership is local execution state, never part of the stored capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedRun {
    pub transfer: RunTransfer,
    pub successor: SegmentOrdinal,
}

impl AdoptedRun {
    /// Rebuild the journal and fence the predecessor before the first append.
    ///
    /// # Errors
    /// A malformed event stream.
    pub fn ledger(&self) -> Result<RunLedger, ContinuationRefusal> {
        let mut ledger = self.transfer.ledger()?;
        ledger.admit_successor(self.successor);
        Ok(ledger)
    }
}

/// Material published by a Run capture. Its lease holder is the capture's
/// `(owner, from)`; storing it here would admit a contradictory second owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferBundle {
    pub artifact: crate::artifact_referrer::ArtifactName,
    pub references: Vec<super::material::MaterialRef>,
    pub copy_bytes: u64,
}

impl From<RetainedBundle> for TransferBundle {
    fn from(bundle: RetainedBundle) -> Self {
        Self {
            artifact: bundle.artifact,
            references: bundle.references,
            copy_bytes: bundle.copy_bytes,
        }
    }
}

impl TransferBundle {
    /// Reconstruct the holder at the store boundary from its capture.
    #[must_use]
    pub fn held_by(&self, holder: MaterialHolder) -> RetainedBundle {
        RetainedBundle {
            holder,
            artifact: self.artifact.clone(),
            references: self.references.clone(),
            copy_bytes: self.copy_bytes,
        }
    }

    fn is_retained(&self) -> bool {
        self.artifact.store == crate::artifact_referrer::ArtifactStoreId::ToolMaterial
            && !self.references.is_empty()
            && self.references.iter().all(|reference| matches!(
                &reference.location,
                super::material::MaterialLocation::RetainedArtifact { artifact } if artifact == &self.artifact
            ))
    }
}
