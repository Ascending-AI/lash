//! K2: owner-qualified material references (binding Q4).
//!
//! A reference names one canonical payload: the Run, process or source that
//! owns it, the record role that produced it, where it lives and the digest
//! its bytes must match. Coordination records hold references, never copies.
//! There is no opaque capability handle and no public resolution API: a
//! failed read is a typed [`MaterialRefusal`], and it never re-executes the
//! body that produced the material.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ProcessId;
use crate::artifact_referrer::ArtifactName;
use crate::await_event_identity::AwaitEventKey;
use crate::effect_opener::EffectOpener;
use crate::store::plugin_writers::PluginRevision;

/// Who owns a payload: the logical Run that admitted it, the process that
/// produced it, or the Deferred source whose seal resolved to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialOwner {
    /// The logical Run, named by its opener.
    Run { opener: EffectOpener },
    /// One process.
    Process { process_id: ProcessId },
    /// One Deferred source, named by its durable wait key.
    Source { source: AwaitEventKey },
}

/// The record that produced a payload, which is also its sole canonical
/// owner record: A owns the prepared request, X the attempt output and its
/// captures, V only presentation bytes distinct from the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialRole {
    /// The final prepared request admission recorded (A).
    PreparedRequest,
    /// One attempt's output and captures (X), or a cached success a
    /// before-check supplied in its place.
    AttemptOutput,
    /// Presentation bytes distinct from the output (V).
    Presentation,
}

/// Where the bytes live.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialLocation {
    /// In the owning Run's opener journal: resolvable within the same
    /// segment only.
    JournalLocal,
    /// In a retained artifact store, held by a dependency lease: resolvable
    /// across segments and deployments.
    RetainedArtifact { artifact: ArtifactName },
}

/// The integrity digest of the material's canonical bytes: 64 lowercase
/// hexadecimal digits of a BLAKE3 hash. The hashing domain belongs to the
/// material codec (FIG-4876), not to this reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MaterialDigest(String);

/// A string that is not a [`MaterialDigest`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a material digest is 64 lowercase hexadecimal digits; got {0:?}")]
pub struct InvalidMaterialDigest(String);

impl MaterialDigest {
    /// `text` as a digest.
    ///
    /// # Errors
    ///
    /// [`InvalidMaterialDigest`] for anything but 64 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, InvalidMaterialDigest> {
        let valid = text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidMaterialDigest(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MaterialDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for MaterialDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A typed reference to one canonical payload.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialRef {
    pub owner: MaterialOwner,
    pub role: MaterialRole,
    pub location: MaterialLocation,
    pub digest: MaterialDigest,
}

impl MaterialRef {
    /// Whether a successor segment can resolve this reference. A
    /// journal-local payload must be retained, and its dependency lease
    /// acquired, before any continuation or source publishes the reference.
    #[must_use]
    pub fn crosses_segments(&self) -> bool {
        matches!(self.location, MaterialLocation::RetainedArtifact { .. })
    }

    /// The same payload after retention into `artifact`. Owner, role and
    /// digest are unchanged: retention moves bytes, never identity. The
    /// copy is a counted handover artifact, not a second canonical owner.
    #[must_use]
    pub fn retained(&self, artifact: ArtifactName) -> Self {
        Self {
            location: MaterialLocation::RetainedArtifact { artifact },
            ..self.clone()
        }
    }

    /// Check a resolved payload against this reference: the reader expected
    /// `owner`, and the bytes it found hash to `found`.
    ///
    /// # Errors
    ///
    /// [`MaterialRefusal::WrongOwner`] before the digest is compared, then
    /// [`MaterialRefusal::Corrupt`].
    pub fn verify(
        &self,
        owner: &MaterialOwner,
        found: &MaterialDigest,
    ) -> Result<(), MaterialRefusal> {
        if &self.owner != owner {
            return Err(MaterialRefusal::WrongOwner {
                reference: Box::new(self.clone()),
                expected: Box::new(owner.clone()),
            });
        }
        if &self.digest != found {
            return Err(MaterialRefusal::Corrupt {
                reference: Box::new(self.clone()),
                found: found.clone(),
            });
        }
        Ok(())
    }
}

/// Why recorded material cannot be served: a typed retained-result failure.
/// None of these re-executes the body that produced the material. FIG-4876
/// carries it through the runtime error framework.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialRefusal {
    /// No payload exists at the reference's location.
    #[error("recorded material {} is missing", reference.digest)]
    Missing { reference: Box<MaterialRef> },
    /// The payload's retention ended; an expired reference cannot restart
    /// work.
    #[error("recorded material {} was retired", reference.digest)]
    Retired { reference: Box<MaterialRef> },
    /// The bytes found do not hash to the recorded digest.
    #[error("recorded material {} is corrupt: found {found}", reference.digest)]
    Corrupt {
        reference: Box<MaterialRef>,
        found: MaterialDigest,
    },
    /// The reader's owner is not the reference's owner.
    #[error("recorded material {} belongs to another owner", reference.digest)]
    WrongOwner {
        reference: Box<MaterialRef>,
        expected: Box<MaterialOwner>,
    },
    /// The material's codec belongs to a plugin revision this build cannot
    /// read.
    #[error("recorded material {} needs an unavailable plugin revision", reference.digest)]
    RevisionMismatch {
        reference: Box<MaterialRef>,
        recorded: PluginRevision,
        available: Vec<PluginRevision>,
    },
}

impl MaterialRefusal {
    /// The stable snake-case code of the refusal.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "material_missing",
            Self::Retired { .. } => "material_retired",
            Self::Corrupt { .. } => "material_corrupt",
            Self::WrongOwner { .. } => "material_wrong_owner",
            Self::RevisionMismatch { .. } => "material_revision_mismatch",
        }
    }
}
