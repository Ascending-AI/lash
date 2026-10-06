//! K2/K4: retained material bundles and the dependency leases that keep
//! them readable (FIG-4889).
//!
//! Material a Deferred source seal names is retained first: its payloads are
//! written once, as one immutable bundle, together with the source's lease,
//! and only then may the seal publish the bundle's references. A bundle
//! retires atomically, every payload at once, when its last lease ends; an
//! ended holder stays fenced, so its retired references refuse typed and
//! never restart work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::material::{
    MaterialDigest, MaterialOwner, MaterialPayload, MaterialRef, MaterialRefusal,
};
use crate::artifact_referrer::{ArtifactName, ArtifactReferrer, ArtifactStoreId};
use crate::await_event_identity::AwaitEventKey;
use crate::runtime_error::RuntimeEffectControllerError;
use crate::store::plugin_writers::PluginRevision;

/// version_surface = "coexist"
/// version_guard(items(BUNDLE_DOMAIN, of), roots(BundleWire))
const BUNDLE_DOMAIN: &str = "lash-tool-material-bundle/v1";
/// version_surface = "coexist"
/// version_guard(items(BUNDLE_REF_PREFIX, of))
const BUNDLE_REF_PREFIX: &str = "tool-material:v1:";

/// Who holds a dependency lease on retained material.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "holder", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialHolder {
    /// One Deferred source: the lease behind its `Resolved` seal's result.
    Source { source: AwaitEventKey },
}

impl MaterialHolder {
    /// The referrer whose edges are this holder's leases.
    #[must_use]
    pub fn referrer(&self) -> ArtifactReferrer {
        match self {
            Self::Source { source } => ArtifactReferrer::Source(Box::new(source.clone())),
        }
    }
}

/// The stored bytes of one bundle: every payload under its digest.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleWire {
    payloads: BTreeMap<MaterialDigest, MaterialPayload>,
}

/// One immutable bundle of canonical payloads, ready to retain. Its name is
/// derived from its bytes, so retaining it twice is the same fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialBundle {
    artifact: ArtifactName,
    bytes: Vec<u8>,
    references: Vec<MaterialRef>,
}

impl MaterialBundle {
    /// The bundle of `payloads`, deduplicated by digest, or `None` when there
    /// is nothing to retain: material that stays in its opener journal needs
    /// no artifact transaction.
    ///
    /// # Errors
    ///
    /// `RecordEncodingFailed` when a payload does not encode.
    pub fn of(
        payloads: impl IntoIterator<Item = MaterialPayload>,
    ) -> Result<Option<Self>, RuntimeEffectControllerError> {
        let mut wire = BundleWire {
            payloads: BTreeMap::new(),
        };
        for payload in payloads {
            let digest = payload
                .reference(super::material::MaterialLocation::JournalLocal)?
                .digest;
            wire.payloads.insert(digest, payload);
        }
        if wire.payloads.is_empty() {
            return Ok(None);
        }
        let bytes = serde_json::to_vec(&wire).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                error.to_string(),
            )
        })?;
        let artifact = ArtifactName {
            store: ArtifactStoreId::ToolMaterial,
            artifact_ref: format!(
                "{BUNDLE_REF_PREFIX}{}",
                lash_sansio::core_support::blake3_domain_hash_hex(BUNDLE_DOMAIN, &bytes)
            ),
        };
        let references = wire
            .payloads
            .values()
            .map(|payload| {
                payload.reference(super::material::MaterialLocation::RetainedArtifact {
                    artifact: artifact.clone(),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(Self {
            artifact,
            bytes,
            references,
        }))
    }

    /// The artifact the bundle is retained as.
    #[must_use]
    pub fn artifact(&self) -> &ArtifactName {
        &self.artifact
    }

    /// The bytes a store writes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Every payload's reference at its retained location, in digest order.
    #[must_use]
    pub fn references(&self) -> &[MaterialRef] {
        &self.references
    }

    /// The bundle as retained under `holder`'s lease. Only a store's retain
    /// mints this, after the lease commits.
    #[must_use]
    pub fn retained_by(&self, holder: MaterialHolder) -> RetainedBundle {
        RetainedBundle {
            holder,
            artifact: self.artifact.clone(),
            references: self.references.clone(),
            copy_bytes: self.bytes.len() as u64,
        }
    }

    /// Serve `reference` out of stored bundle `bytes`, read under a lease,
    /// verified against `owner`, its integrity, format and `available`
    /// codec revisions.
    ///
    /// # Errors
    ///
    /// A typed [`MaterialRefusal`] (`Missing` when the bundle holds no payload
    /// with the reference's digest, otherwise as
    /// [`MaterialPayload::verify`]), or `RuntimeStoreCorrupt` for bytes that
    /// are not a bundle.
    pub fn read(
        bytes: &[u8],
        reference: &MaterialRef,
        owner: &MaterialOwner,
        available: &[PluginRevision],
    ) -> Result<MaterialPayload, RuntimeEffectControllerError> {
        let mut wire: BundleWire = serde_json::from_slice(bytes).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("retained tool material is not a bundle: {error}"),
            )
        })?;
        let payload =
            wire.payloads
                .remove(&reference.digest)
                .ok_or_else(|| MaterialRefusal::Missing {
                    reference: Box::new(reference.clone()),
                })?;
        payload.verify(reference, owner, available)?;
        Ok(payload)
    }
}

/// A bundle retained under one holder's lease: what a continuation or seal
/// publishes. Every reference is resolvable across segments for as long as
/// some holder's lease on `artifact` lasts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedBundle {
    /// The holder whose lease was committed before publication.
    pub holder: MaterialHolder,
    pub artifact: ArtifactName,
    pub references: Vec<MaterialRef>,
    /// The handover copy the bundle cost, in bytes: measured, and not a
    /// second canonical owner (L15).
    pub copy_bytes: u64,
}

impl RetainedBundle {
    /// The same bundle as held by `holder`, once its lease commits.
    #[must_use]
    pub fn held_by(&self, holder: MaterialHolder) -> Self {
        Self {
            holder,
            ..self.clone()
        }
    }

    /// Whether this is a tool-material bundle whose every reference lives
    /// in it.
    #[must_use]
    pub fn is_retained(&self) -> bool {
        self.artifact.store == ArtifactStoreId::ToolMaterial
            && !self.references.is_empty()
            && self.references.iter().all(|reference| {
                matches!(
                    &reference.location,
                    super::material::MaterialLocation::RetainedArtifact { artifact }
                        if artifact == &self.artifact
                )
            })
    }
}

/// Why a store refused a retention operation.
#[derive(Debug, thiserror::Error)]
pub enum MaterialRetentionError {
    /// A typed retained-result failure: the material cannot be served and no
    /// body re-executes.
    #[error(transparent)]
    Refused(#[from] MaterialRefusal),
    /// The holder's leases ended; it can neither retain, acquire nor read.
    #[error("material holder `{}` has ended", holder.referrer())]
    HolderEnded { holder: Box<MaterialHolder> },
    #[error(transparent)]
    Controller(Box<RuntimeEffectControllerError>),
    #[error(transparent)]
    Store(#[from] crate::StoreError),
}

impl From<RuntimeEffectControllerError> for MaterialRetentionError {
    fn from(error: RuntimeEffectControllerError) -> Self {
        match error.cause {
            Some(crate::RuntimeErrorCause::MaterialRefused { refusal }) => Self::Refused(*refusal),
            _ => Self::Controller(Box::new(error)),
        }
    }
}
