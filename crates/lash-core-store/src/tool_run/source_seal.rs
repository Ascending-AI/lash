//! K4: the immutable seal of a Deferred source (FIG-4883 implements it on
//! the surviving durable-wait workflow).
//!
//! A source ends exactly once, `Resolved(ref)` or `Cancelled`; there is no
//! deadline and no timeout outcome (binding Q3). The first authenticated
//! write wins and later writes read the existing seal. A Run waits through
//! a short subscription naming the segment to wake, never through a long
//! waiting invocation. Ranking and drain are the Run's own events
//! ([`super::run_event`]); nothing claims one transaction across the seal
//! and the Run.

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::admission::ExternalCancelPolicy;
use super::material::{MaterialLocation, MaterialOwner, MaterialRef};
use super::run_event::SegmentOrdinal;
use crate::ProcessId;
use crate::artifact_referrer::ArtifactStoreId;
use crate::await_event_identity::AwaitEventKey;
use crate::effect_opener::EffectOpener;
use crate::store::plugin_writers::PluginRevision;

/// Who may resolve a source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceAuthority {
    /// The terminal of one process.
    ProcessTerminal { process_id: ProcessId },
    /// An external actor completing the call's reserved completion key.
    ExternalCompletion,
}

/// The writer a seal write authenticates as.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "writer", rename_all = "snake_case", deny_unknown_fields)]
pub enum SealWriter {
    /// A process publishing its terminal.
    Process { process_id: ProcessId },
    /// An external completion of the reserved key.
    External,
    /// The owning Run, cancelling.
    Owner { opener: EffectOpener },
}

/// What a source records when its call parks on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDescriptor {
    pub source: AwaitEventKey,
    pub call_id: ToolCallId,
    /// The logical Run that owns the call.
    pub owner: EffectOpener,
    /// The plugin revision whose finalization runs when the source
    /// resolves.
    pub resolver: PluginRevision,
    pub authority: SourceAuthority,
    pub cancel: ExternalCancelPolicy,
}

/// A source's one terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "seal", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSeal {
    /// The resolved result, owned by the source.
    Resolved {
        result: Box<MaterialRef>,
    },
    Cancelled,
}

/// The answer to a seal write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SealOutcome {
    /// This write sealed the source.
    Sealed { seal: SourceSeal },
    /// An earlier write sealed it; this is that seal, unchanged.
    AlreadySealed { seal: SourceSeal },
}

/// Why a seal write was refused before it could seal anything.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum SealRefusal {
    #[error("the writer has no authority over this source")]
    WrongAuthority,
    #[error("the resolved result is not owned by its source")]
    ResultNotOwned,
    /// A seal is read by another segment, so its result must already be
    /// retained, with the source's lease, before the seal publishes it.
    #[error("the resolved result is not retained material")]
    UnretainedResult,
}

/// Why the index refused a source request; nothing was written.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRefusal {
    /// The source's scope was retired: no source of it is armed, subscribed
    /// or sealed again.
    #[error("the source's scope is retired")]
    Retired,
    /// No Run armed this source, or its Run retired it.
    #[error("the source is not armed")]
    NotArmed,
    /// The source is armed with another descriptor.
    #[error("the source is armed with another descriptor")]
    DescriptorMismatch,
    /// A subscription named a Run other than the source's owner.
    #[error("the subscription names a Run that does not own the source")]
    WrongOwner,
    /// The pinned descriptor refused the write.
    #[error(transparent)]
    Seal { seal: SealRefusal },
}

impl From<SourceRefusal> for crate::runtime_error::RuntimeEffectControllerError {
    fn from(refusal: SourceRefusal) -> Self {
        let mut error = Self::new(
            crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
            refusal.to_string(),
        );
        error.cause = Some(crate::RuntimeErrorCause::SourceRefused {
            refusal: Box::new(refusal),
        });
        error.journaled = true;
        error
    }
}

impl SourceDescriptor {
    /// Authenticate `writer`'s proposal and apply first-writer-wins over
    /// `existing`. An authenticated late write, including one after a
    /// cancellation, reads the existing seal and revives nothing.
    ///
    /// # Errors
    ///
    /// [`SealRefusal`] for a writer without authority, a result another
    /// owner holds or a result not yet retained; the source is unchanged.
    pub fn seal(
        &self,
        existing: Option<&SourceSeal>,
        writer: &SealWriter,
        proposed: SourceSeal,
    ) -> Result<SealOutcome, SealRefusal> {
        let authorized = match (&proposed, writer) {
            (SourceSeal::Cancelled, SealWriter::Owner { opener }) => opener == &self.owner,
            (SourceSeal::Resolved { result }, writer) => {
                if result.owner
                    != (MaterialOwner::Source {
                        source: self.source.clone(),
                    })
                {
                    return Err(SealRefusal::ResultNotOwned);
                }
                if !matches!(
                    &result.location,
                    MaterialLocation::RetainedArtifact { artifact }
                        if artifact.store == ArtifactStoreId::ToolMaterial
                ) {
                    return Err(SealRefusal::UnretainedResult);
                }
                matches!(
                    (&self.authority, writer),
                    (SourceAuthority::ExternalCompletion, SealWriter::External)
                ) || matches!(
                    (&self.authority, writer),
                    (
                        SourceAuthority::ProcessTerminal { process_id },
                        SealWriter::Process { process_id: by },
                    ) if process_id == by
                )
            }
            (SourceSeal::Cancelled, _) => false,
        };
        if !authorized {
            return Err(SealRefusal::WrongAuthority);
        }
        Ok(match existing {
            Some(seal) => SealOutcome::AlreadySealed { seal: seal.clone() },
            None => SealOutcome::Sealed { seal: proposed },
        })
    }
}

/// A short-lived subscription a segment holds on a source: the seal wakes
/// that segment of the owning Run. It transfers with the Run's
/// continuation; it is not a long waiting invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSubscription {
    pub source: AwaitEventKey,
    pub owner: EffectOpener,
    pub segment: SegmentOrdinal,
}
